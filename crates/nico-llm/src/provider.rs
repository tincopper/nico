//! The provider contract: how one upstream's wire format is addressed, encoded and decoded.
//!
//! An adapter owns exactly one provider's wire format and nothing else. It is handed the compiled
//! route facts ([`ProviderRoute`]) and a canonical request, and answers with a destination, a path,
//! headers and a body; it decodes the provider's response or stream back into canonical values. It
//! does not open connections, apply TLS policy, or own a timeout, a budget or a retry — those belong
//! to the data plane, and an adapter that did them would be a second, unaccounted network stack.
//!
//! Two invariants shape the trait:
//!
//! - **`endpoint` depends on the route, never on the request body.** The data plane can therefore
//!   send the upstream head before the body is decoded. A provider whose path needs a value from the
//!   body would break that; such a provider needs a route-level fact, not a pre-body exchange
//!   smuggled in here.
//! - **One adapter per process, one stream per response.** The trait is object-safe, every method
//!   takes `&self`, and the only mutable state a response needs lives in the [`ProviderStream`] that
//!   [`Provider::stream_decoder`] returns.

use std::{borrow::Cow, collections::BTreeMap, fmt, sync::Arc};

use nico_core::ErrorCategory;

use crate::{
    protocol::{Capabilities, Capability, ProtocolId, RegistryError, ResponseFacts},
    schema::{CanonicalError, ChatRequest, ChatResponse, StreamEvent},
};

mod endpoint;
mod route;

pub mod openai;

pub use endpoint::{Endpoint, EndpointError, WireRequest};
pub use openai::OpenAiProvider;
pub use route::{Credential, ExchangeMode, ProviderRoute, RouteError};

/// One provider's identifier, for example `vendor`; the value is also its registry key.
///
/// A kind is not a protocol: one provider can speak more than one wire dialect, and a route names the
/// kind it wants served while its `protocol` names the client side. Keeping the two as distinct types
/// makes it impossible to index the codec registry with a provider kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProviderKind(&'static str);

impl ProviderKind {
    /// Names a provider kind.
    #[must_use]
    pub const fn new(kind: &'static str) -> Self {
        Self(kind)
    }

    /// Returns the registry key.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        self.0
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

/// Why a provider adapter could not translate one document.
///
/// The exchange maps this into a [`CanonicalError`] (see [`ExchangeError`](crate::ExchangeError)),
/// which is the only place that decides the status and stable code a client sees. `reason` is a
/// parser diagnostic for the log; it never carries the provider's body or a credential, and a
/// provider's own error envelope is read into [`ProviderError::Upstream`] instead of being echoed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ProviderError {
    /// This adapter cannot express something the canonical request carries.
    #[error("`{kind}` cannot express `{feature}`")]
    Unsupported {
        /// The adapter that lacks the feature.
        kind: ProviderKind,
        /// The capability the canonical value needs.
        feature: Capability,
    },
    /// This adapter cannot carry a canonical value in the position it appears in.
    ///
    /// A [`Capability`] names a feature the canonical shapes can express, not where they may appear:
    /// this protocol accepts an image in a user turn but not in a system, assistant or tool turn, and
    /// no capability says "images here". The refusal names the canonical field instead of inventing
    /// an axis the request path could never compute, and it is the same refusal as
    /// [`ProviderError::Unsupported`]: the document is well formed and this route cannot serve it.
    #[error("`{kind}` cannot express the field `{field}`")]
    UnsupportedField {
        /// The adapter that cannot carry the field.
        kind: ProviderKind,
        /// Dotted path of the canonical field, without any part of its value.
        field: Cow<'static, str>,
    },
    /// The canonical request cannot be encoded for this provider.
    #[error("`{kind}` could not encode the canonical request: {reason}")]
    InvalidRequest {
        /// The adapter whose encoder failed.
        kind: ProviderKind,
        /// A diagnostic, for the log only.
        reason: String,
    },
    /// The route facts do not describe an addressable endpoint.
    #[error("`{kind}` reports an unusable endpoint")]
    InvalidEndpoint {
        /// The adapter that could not build the endpoint.
        kind: ProviderKind,
        /// The check that failed.
        #[source]
        source: EndpointError,
    },
    /// The provider's bytes are not a document of its wire protocol.
    #[error("`{kind}` returned a document this adapter cannot read: {reason}")]
    Malformed {
        /// The adapter that could not read the document.
        kind: ProviderKind,
        /// A diagnostic, for the log only.
        reason: String,
    },
    /// The provider answered with a failure status.
    ///
    /// The adapter has already mapped the provider's status and error body into the safe canonical
    /// form, so the exchange can answer the client in the client's own protocol without the provider
    /// body ever reaching it.
    #[error("the upstream reported a failure: {error}")]
    Upstream {
        /// The canonical failure the provider's status and body produce.
        error: CanonicalError,
    },
}

impl ProviderError {
    /// Returns how this failure is classified.
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        match self {
            Self::Unsupported { .. } | Self::UnsupportedField { .. } => ErrorCategory::Client,
            Self::InvalidRequest { .. } | Self::InvalidEndpoint { .. } => ErrorCategory::Internal,
            Self::Malformed { .. } => ErrorCategory::Upstream,
            Self::Upstream { error } => error.category(),
        }
    }
}

/// One provider response stream's decoder.
///
/// The value is created per response by [`Provider::stream_decoder`], so the partial frame it holds
/// between chunks dies with the response instead of leaking into the next one. The decoder emits
/// canonical events, never the provider's chunk objects: a client protocol codec encodes those
/// events back into the bytes its caller understands.
pub trait ProviderStream: Send {
    /// Feeds one arbitrary chunk of the provider's response body.
    ///
    /// The chunk boundary is the transport's, not the protocol's: a frame may be split across two
    /// calls or several frames may arrive in one. The decoder keeps the remainder in its own instance
    /// and returns every event the new bytes complete.
    ///
    /// # Errors
    /// Returns [`ProviderError::Malformed`] when the bytes are not a document of this provider's wire
    /// protocol.
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<StreamEvent>, ProviderError>;

    /// Signals that the response body ended and returns the events it completes.
    ///
    /// # Errors
    /// Returns [`ProviderError::Malformed`] when an incomplete frame is still buffered.
    fn finish(&mut self) -> Result<Vec<StreamEvent>, ProviderError>;
}

/// One provider's wire format.
///
/// An adapter is shared by every route served by its kind and holds configuration only. It receives
/// the route on [`Provider::endpoint`] and [`Provider::encode`] because the physical model, the base
/// URL and the credential are route facts, not request facts: a singleton adapter could not otherwise
/// know which physical model to write, and the canonical request keeps the logical name so the same
/// value can be re-encoded for another route.
pub trait Provider: Send + Sync {
    /// Returns the provider kind this adapter serves; the value is also its registry key.
    fn kind(&self) -> ProviderKind;

    /// Returns the wire protocol this provider speaks natively.
    ///
    /// A route may be served by any client protocol, so this value is not the route's `protocol`:
    /// it names the provider's own wire format. Publication reads it to decide whether a
    /// `passthrough` route is legal — bytes may only skip the canonical model when both sides already
    /// speak the same protocol — and it uses the adapter's own answer rather than a vendor table, so
    /// adding a provider never adds a branch to the plan compiler.
    fn native_protocol(&self) -> ProtocolId;

    /// Returns what this provider can express.
    fn capabilities(&self) -> Capabilities;

    /// Returns the wire revision this adapter is pinned to, when the vendor publishes one.
    ///
    /// The same rule as [`ClientCodec::version`](crate::ClientCodec::version): `None` is the default
    /// because most vendors have no versioned spelling, and the assembly log leaves the field out
    /// rather than inventing a value. An adapter whose vendor does version reports it here — that
    /// revision is then visible per process without a request ever running.
    fn version(&self) -> Option<&'static str> {
        None
    }

    /// Resolves the route's base URL and credential into the endpoint the data plane will contact.
    ///
    /// The result depends on the route only, so it is **compiled once**, when the exchange pairs the
    /// route: the data plane asks the exchange for it before the request body is decoded, which is
    /// what lets the upstream head go out while the client is still uploading, and no request
    /// re-reads the base URL. An unaddressable route is therefore a publication failure, not a
    /// failure every one of its requests would report.
    ///
    /// # Errors
    /// Returns [`ProviderError::InvalidEndpoint`] when the route's base URL or credential cannot form
    /// an endpoint, and [`ProviderError::Unsupported`] when the route names something this provider
    /// cannot serve.
    fn endpoint(&self, route: &ProviderRoute) -> Result<Endpoint, ProviderError>;

    /// Encodes the canonical request as this provider's request body.
    ///
    /// The route is the source of the physical model: [`ProviderRoute::model`] resolves the logical
    /// name the client wrote, and the adapter writes the physical name into the body. The canonical
    /// request itself is not rewritten, so a retry under another route re-resolves the name.
    ///
    /// # Errors
    /// Returns [`ProviderError::Unsupported`] when the request needs a capability this adapter does
    /// not have, [`ProviderError::UnsupportedField`] when a canonical value has no place in this
    /// wire format, and [`ProviderError::InvalidRequest`] when the encoder itself fails.
    fn encode(
        &self,
        route: &ProviderRoute,
        request: &ChatRequest,
    ) -> Result<Vec<u8>, ProviderError>;

    /// Decodes one complete provider response body into the canonical response.
    ///
    /// A failure status is not a response with no choice: the adapter maps the status and its own
    /// error envelope into [`ProviderError::Upstream`], so the exchange can answer the client in the
    /// client's protocol.
    ///
    /// # Errors
    /// Returns [`ProviderError::Upstream`] for a failure status, and [`ProviderError::Malformed`] when
    /// the body is not a document of this provider's protocol or carries something the canonical
    /// model cannot express. `Unsupported` is a request-time refusal and is not used here.
    fn decode_response(
        &self,
        head: &ResponseFacts,
        body: &[u8],
    ) -> Result<ChatResponse, ProviderError>;

    /// Maps a provider failure status into the canonical error **without reading its body**.
    ///
    /// A response's head is written before its body is read, so the client status a failure produces
    /// must be known from the status alone. The exchange uses this while translating a response: a
    /// failure status becomes the client's error answer here, and a buffered body over the route's
    /// ceiling is refused with the status this same table would have produced. The mapping is the
    /// adapter's closed table — the one [`Provider::decode_response`] uses for the same status — so the
    /// two paths cannot disagree about which failure a status means.
    ///
    /// The provider's own error code is only available from its body, so a value produced here carries
    /// none. The caller-visible status, code, category and retryability are unaffected: all four are
    /// decided by the status.
    fn error_for_status(&self, status: u16) -> CanonicalError;

    /// Creates a decoder for one streamed response.
    ///
    /// A failure status is not a stream: the provider's failure envelope is a document, not an event
    /// stream, so the caller must pass it to [`Provider::decode_response`] and hand only a success
    /// body to the decoder. The decoder is not told the status, and feeding it an error document
    /// would report a malformed stream instead of the provider's own failure.
    ///
    /// # Errors
    /// The decoder reports parse failures from [`ProviderStream::push`] and
    /// [`ProviderStream::finish`], not from construction: the first bytes have not arrived yet.
    fn stream_decoder(&self) -> Box<dyn ProviderStream>;
}

/// The provider kinds this process can serve, keyed by [`ProviderKind`].
///
/// Ordered for the same reason [`ProtocolRegistry`](crate::ProtocolRegistry) is: publication logs one
/// line per kind. An unknown kind is a publication failure, and a duplicate registration is refused
/// rather than overwritten, because which adapter a route reached would then depend on assembly
/// order.
#[derive(Default)]
pub struct ProviderRegistry {
    providers: BTreeMap<&'static str, Arc<dyn Provider>>,
}

impl ProviderRegistry {
    /// Creates an empty registry: no route can reach a provider yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Iterates the registered adapters, in registry-key order.
    ///
    /// The counterpart of [`ProtocolRegistry::iter`](crate::ProtocolRegistry::iter): the assembly
    /// reports one line per adapter, and only this value knows which ones exist.
    pub fn iter(&self) -> impl Iterator<Item = &dyn Provider> {
        self.providers.values().map(AsRef::as_ref)
    }

    /// Adds one provider kind's adapter.
    ///
    /// # Errors
    /// Returns [`RegistryError::DuplicateKind`](crate::RegistryError) when an adapter for the same
    /// kind is already registered.
    pub fn register(&mut self, provider: Arc<dyn Provider>) -> Result<(), RegistryError> {
        let kind = provider.kind();
        let key = kind.as_str();
        if self.providers.contains_key(key) {
            return Err(RegistryError::DuplicateKind {
                kind: key.to_owned(),
            });
        }
        self.providers.insert(key, provider);
        Ok(())
    }

    /// Returns the adapter registered for `kind`, or `None` when this process cannot serve it.
    #[must_use]
    pub fn provider(&self, kind: &str) -> Option<&Arc<dyn Provider>> {
        self.providers.get(kind)
    }

    /// Returns how many kinds are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// Reports whether no kind is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

impl fmt::Debug for ProviderRegistry {
    /// Prints the registered kinds, not the adapters: a registry is logged once per publication and
    /// an adapter holds configuration the log has no business printing.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_set()
            .entries(self.providers.keys().copied())
            .finish()
    }
}
