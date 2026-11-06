//! The client-protocol contract: what one codec decodes and encodes, and the registry of codecs a
//! process can serve.
//!
//! A codec owns exactly one client protocol's wire format. It reads a client request into the
//! canonical [`ChatRequest`] and writes canonical values back as that protocol's response, stream
//! and error documents. It never learns which provider serves the request and never names one: the
//! route chooses the provider, and the canonical model is the only vocabulary the two sides share.
//!
//! The two *facts* types are the protocol-independent views of an exchange: a codec that needs the
//! method, the path or a header reads [`RequestFacts`]; a provider adapter that must interpret a
//! response status reads [`ResponseFacts`]. Both are owned, framework-free values (they live in the
//! `facts` submodule), so a codec is testable without a server and the data plane never leaks a
//! framework type into this contract. [`ClientReply`] is what an encoder produces for the client.

use std::{collections::BTreeMap, fmt, sync::Arc};

use crate::schema::{CanonicalError, ChatRequest, ChatResponse, StreamEvent};

mod facts;

pub(crate) use facts::header_names;
pub use facts::{RequestFacts, ResponseFacts};

/// One client protocol's identifier, for example `vendor.chat`.
///
/// The value is the registry key and a log field, so it is a `'static` string rather than a
/// formatted one: a codec's protocol never depends on a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProtocolId(&'static str);

impl ProtocolId {
    /// Names a protocol.
    #[must_use]
    pub const fn new(id: &'static str) -> Self {
        Self(id)
    }

    /// Returns the registry key.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        self.0
    }
}

impl fmt::Display for ProtocolId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

/// One optional feature a client protocol or a provider adapter may support.
///
/// The set is deliberately small and semantic: these are the features a request can require, so a
/// capability the pair does not share is a refusal with a stable name instead of a silently dropped
/// field. Plain text needs no capability; it is what a chat protocol does by definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Capability {
    /// Incremental, event-streamed responses.
    Streaming,
    /// Tool declarations, tool calls and tool results.
    Tools,
    /// Image content parts.
    Images,
    /// File content parts.
    Files,
    /// Hidden reasoning parts.
    Reasoning,
    /// A structured response format.
    ResponseFormat,
}

/// Number of kinds in [`Capability::ALL`]; the storage width of [`Capabilities`].
const CAPABILITY_COUNT: usize = 6;

// The bit set has one bit per capability, so the table must fit the `u8` it is stored in.
const _: () = assert!(CAPABILITY_COUNT <= u8::BITS as usize);

impl Capability {
    /// Every capability, in bit order.
    pub const ALL: [Self; CAPABILITY_COUNT] = [
        Self::Streaming,
        Self::Tools,
        Self::Images,
        Self::Files,
        Self::Reasoning,
        Self::ResponseFormat,
    ];

    /// Returns the capability's position in [`Capability::ALL`].
    const fn index(self) -> usize {
        match self {
            Self::Streaming => 0,
            Self::Tools => 1,
            Self::Images => 2,
            Self::Files => 3,
            Self::Reasoning => 4,
            Self::ResponseFormat => 5,
        }
    }

    /// Returns the bit this capability occupies.
    const fn mask(self) -> u8 {
        1 << self.index()
    }

    /// Returns the stable name used in logs and diagnostics.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Streaming => "streaming",
            Self::Tools => "tools",
            Self::Images => "images",
            Self::Files => "files",
            Self::Reasoning => "reasoning",
            Self::ResponseFormat => "response_format",
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// A set of [`Capability`] flags.
///
/// One value answers "what can this side express". Publication intersects the client codec's set with
/// the provider adapter's and the exchange keeps the result, so a request that needs something the
/// pair does not share is refused once, with the capability named, instead of failing later inside a
/// vendor encoder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Capabilities {
    bits: u8,
}

impl Capabilities {
    /// Builds a set with no capability.
    #[must_use]
    pub const fn none() -> Self {
        Self { bits: 0 }
    }

    /// Builds a set with every capability.
    #[must_use]
    pub const fn all() -> Self {
        Self {
            bits: (1 << CAPABILITY_COUNT) - 1,
        }
    }

    /// Reports whether `capability` is set.
    #[must_use]
    pub const fn supports(self, capability: Capability) -> bool {
        self.bits & capability.mask() != 0
    }

    /// Returns a copy with `capability` set.
    #[must_use]
    pub const fn with(mut self, capability: Capability) -> Self {
        self.bits |= capability.mask();
        self
    }

    /// Returns a copy with `capability` cleared.
    #[must_use]
    pub const fn without(mut self, capability: Capability) -> Self {
        self.bits &= !capability.mask();
        self
    }

    /// Returns the capabilities both sets have.
    #[must_use]
    pub const fn intersection(self, other: Self) -> Self {
        Self {
            bits: self.bits & other.bits,
        }
    }

    /// Reports whether no capability is set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.bits == 0
    }

    /// Iterates the capabilities that are set, in [`Capability::ALL`] order.
    pub fn iter(self) -> impl Iterator<Item = Capability> {
        Capability::ALL
            .into_iter()
            .filter(move |capability| self.supports(*capability))
    }
}

/// Why a codec could not translate one document.
///
/// A codec shapes this into a [`CanonicalError`] at the exchange boundary (see
/// [`ExchangeError`](crate::ExchangeError)), which is the only place that decides the status and the
/// stable code a client sees. `reason` is a parser diagnostic for the log: it never carries the
/// request body, a header value or a credential, and it is never written to the client.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CodecError {
    /// The body or a required fact is not a valid document of this protocol.
    #[error("invalid `{protocol}` request: {reason}")]
    InvalidRequest {
        /// The protocol whose document failed to decode.
        protocol: ProtocolId,
        /// A parser diagnostic, for the log only.
        reason: String,
    },
    /// This client protocol cannot express a request field the caller sent.
    ///
    /// It is a request-time refusal: the client asked for something this protocol cannot carry, so
    /// the exchange answers `422`. A response value the codec cannot write is `Encode`, not this.
    #[error("`{protocol}` cannot express `{feature}`")]
    Unsupported {
        /// The protocol that lacks the feature.
        protocol: ProtocolId,
        /// The capability the canonical value needs.
        feature: Capability,
    },
    /// The canonical value cannot be written as this protocol's wire format.
    #[error("`{protocol}` could not encode a canonical value: {reason}")]
    Encode {
        /// The protocol whose encoder failed.
        protocol: ProtocolId,
        /// A diagnostic, for the log only.
        reason: String,
    },
}

/// One client protocol's wire format.
///
/// A codec is shared by every request of its protocol, so it holds configuration and no
/// per-request state: the decode result is a value the exchange keeps, not state the codec
/// remembers. Implementations live one per file under `protocol/`; the registry below is the only
/// place protocol names are resolved.
pub trait ClientCodec: Send + Sync {
    /// Returns the protocol this codec speaks; the value is also its registry key.
    fn protocol(&self) -> ProtocolId;

    /// Returns what this protocol can express.
    fn capabilities(&self) -> Capabilities;

    /// Decodes one buffered client body and the facts around it into the canonical request.
    ///
    /// The body is complete: a convert-mode exchange buffers it before calling here, so a codec may
    /// parse it as one document. A field the canonical model does not name belongs in
    /// [`ChatRequest::extensions`]; a field the codec cannot express is refused with
    /// [`CodecError::Unsupported`], never dropped.
    ///
    /// # Errors
    /// Returns [`CodecError::InvalidRequest`] when the document is malformed or a required fact is
    /// missing, and [`CodecError::Unsupported`] when the request needs something this protocol cannot
    /// express.
    fn decode_request(&self, facts: &RequestFacts, body: &[u8]) -> Result<ChatRequest, CodecError>;

    /// Encodes one complete canonical response as this protocol's answer.
    ///
    /// The reply carries the status the client sees, because the status of a *successful* answer is a
    /// property of the client protocol. A provider's own 2xx variant describes how the provider
    /// framed its body, so it must not become the caller's status once the body has been re-encoded.
    /// A failure is the other way round: its status belongs to [`CanonicalError`], and
    /// [`ClientCodec::encode_error`] returns only the body.
    ///
    /// # Errors
    /// Returns [`CodecError::Encode`] when the protocol's encoder fails. A response value the
    /// protocol cannot express is also an `Encode` failure: a response is not a client request, so it
    /// cannot be refused as unsupported after the upstream has answered.
    fn encode_response(&self, response: &ChatResponse) -> Result<ClientReply, CodecError>;

    /// Encodes one canonical stream event as the bytes the client receives for it.
    ///
    /// The returned bytes include whatever framing the protocol needs — concatenating the results of
    /// a sequence of events is a well-formed stream of this protocol.
    ///
    /// # Errors
    /// Returns [`CodecError::Encode`] when the encoder fails, including for an event the protocol
    /// cannot express: once the stream has started there is no status left to refuse with.
    fn encode_stream_event(&self, event: &StreamEvent) -> Result<Vec<u8>, CodecError>;

    /// Encodes one canonical failure in this protocol's error envelope.
    ///
    /// # Errors
    /// Returns [`CodecError::Encode`] when the protocol has no error envelope the encoder can build.
    fn encode_error(&self, error: &CanonicalError) -> Result<Vec<u8>, CodecError>;
}

/// What one client protocol's encoder produces: the status the client is answered with and the
/// bytes that carry it.
///
/// The type belongs to the protocol layer, not to the exchange, because the status of a complete
/// answer is a property of the client protocol: a chat protocol answers a finished response with
/// `200`, and a provider's own `201`/`206` describes how the provider framed *its* body rather than
/// what the caller asked for. A failure reply is assembled by the exchange from
/// [`CanonicalError::http_status`], which already owns the status of a failure.
#[derive(Clone, PartialEq, Eq)]
pub struct ClientReply {
    status: u16,
    body: Vec<u8>,
}

impl fmt::Debug for ClientReply {
    /// Prints the status and the body's length, never the body: it is the model's answer.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientReply")
            .field("status", &self.status)
            .field("body_len", &self.body.len())
            .finish()
    }
}

impl ClientReply {
    /// Pairs a status with the body an encoder produced.
    ///
    /// `status` must be one the data plane can write: a codec owns that choice for a success, and
    /// the exchange passes [`CanonicalError::http_status`] for a failure.
    #[must_use]
    pub fn new(status: u16, body: Vec<u8>) -> Self {
        Self { status, body }
    }

    /// Returns the HTTP status to answer with.
    #[must_use]
    pub const fn status(&self) -> u16 {
        self.status
    }

    /// Returns the encoded body.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// Consumes the reply and returns its body.
    #[must_use]
    pub fn into_body(self) -> Vec<u8> {
        self.body
    }
}

/// Why a registry refused a registration.
///
/// Both [`ProtocolRegistry`] and [`ProviderRegistry`](crate::ProviderRegistry) return it: a
/// duplicate identifier makes lookup depend on assembly order, whichever map holds it. The set may
/// grow, so matches outside this crate must name the variants they handle plus a wildcard.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RegistryError {
    /// Two implementations claim the same identifier.
    #[error("`{kind}` is registered twice")]
    DuplicateKind {
        /// The identifier both implementations answer to.
        kind: String,
    },
}

/// The client protocols this process can serve, keyed by [`ProtocolId`].
///
/// Ordered because publication logs one line per protocol and a deterministic order makes two runs
/// of the same document comparable. Lookup takes a `&str` so a document's `protocol` field can be
/// resolved without allocating; an unknown protocol is a publication failure, never a request-time
/// surprise.
#[derive(Default)]
pub struct ProtocolRegistry {
    codecs: BTreeMap<&'static str, Arc<dyn ClientCodec>>,
}

impl ProtocolRegistry {
    /// Creates an empty registry: no client protocol can be served yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one client protocol's codec.
    ///
    /// # Errors
    /// Returns [`RegistryError::DuplicateKind`] when a codec for the same protocol is already
    /// registered. Replacing it instead would make the implementation a request reaches depend on
    /// assembly order.
    pub fn register(&mut self, codec: Arc<dyn ClientCodec>) -> Result<(), RegistryError> {
        let protocol = codec.protocol();
        let key = protocol.as_str();
        if self.codecs.contains_key(key) {
            return Err(RegistryError::DuplicateKind {
                kind: key.to_owned(),
            });
        }
        self.codecs.insert(key, codec);
        Ok(())
    }

    /// Returns the codec registered for `protocol`, or `None` when this process cannot speak it.
    #[must_use]
    pub fn codec(&self, protocol: &str) -> Option<&Arc<dyn ClientCodec>> {
        self.codecs.get(protocol)
    }

    /// Returns how many protocols are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.codecs.len()
    }

    /// Reports whether no protocol is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.codecs.is_empty()
    }
}

impl fmt::Debug for ProtocolRegistry {
    /// Prints the registered protocol names, not the codecs: a registry is logged once per
    /// publication and a codec's fields are none of the log's business.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_set()
            .entries(self.codecs.keys().copied())
            .finish()
    }
}
