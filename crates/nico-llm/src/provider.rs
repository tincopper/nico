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
use nico_http::Destination;

use crate::{
    protocol::{Capabilities, Capability, RegistryError, ResponseFacts, header_names},
    schema::{CanonicalError, ChatRequest, ChatResponse, StreamEvent},
};

mod route;

pub mod openai;

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

/// Why an [`Endpoint`] could not be built.
///
/// The checks exist because the parts can come from a configuration document and a credential file:
/// a path that is not absolute, a header name that is not a token, or a header value holding a line
/// break would let a value the gateway carries split one request into two on the wire.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EndpointError {
    /// The path is not an absolute request target, or holds a control character.
    #[error("the endpoint path must be absolute and free of control characters")]
    Path,
    /// A route's `base_url` is not an absolute `http(s)` URL an adapter can address.
    ///
    /// The adapter refuses the forms it would otherwise have to reinterpret: a missing or unsupported
    /// scheme, user information (a credential belongs in `credential_ref`), a query or fragment, an
    /// empty host, a host that is not a name (see [`nico_core::validate_host`]), a port that is not a
    /// number, is zero, or is out of range, and whitespace or a control character anywhere. `reason`
    /// is a description of the rule that broke, never the value itself: a rejected URL can carry user
    /// information, and a diagnostic must not quote it.
    #[error("`{field}` is not an addressable URL: {reason}")]
    BaseUrl {
        /// Dotted path of the configuration field.
        field: &'static str,
        /// The rule the value broke, as a diagnostic.
        reason: Cow<'static, str>,
    },
    /// A header name is not an HTTP token.
    #[error("`{name}` is not a valid header name")]
    HeaderName {
        /// The rejected name.
        name: String,
    },
    /// A header value holds a control character.
    #[error("the value of header `{name}` holds a control character")]
    HeaderValue {
        /// The header whose value was rejected.
        name: String,
    },
}

/// Where one upstream request goes and what it presents: destination, path and headers.
///
/// A chat exchange is always a `POST`; the data plane owns the method and the framing, so an endpoint
/// carries only what the provider decides. That ownership is also why a provider must not set
/// `host`, `content-length`, `transfer-encoding` or a hop-by-hop header here: the data plane derives
/// them from the destination and the body it actually writes. The path is absolute and is derived
/// from route facts, so the data plane can send the head before the request body is available.
#[derive(Clone, PartialEq, Eq)]
pub struct Endpoint {
    destination: Destination,
    path: String,
    headers: Vec<(String, String)>,
}

impl fmt::Debug for Endpoint {
    /// Names the headers and never prints their values: one of them is the provider credential.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Endpoint")
            .field("destination", &self.destination)
            .field("path", &self.path)
            .field("headers", &header_names(&self.headers))
            .finish()
    }
}

impl Endpoint {
    /// Builds an endpoint and checks that the data plane may put it on the wire.
    ///
    /// # Errors
    /// Returns [`EndpointError::Path`] for a path that is not absolute or holds a control character,
    /// [`EndpointError::HeaderName`] for a name that is not a token, and
    /// [`EndpointError::HeaderValue`] for a value that holds a control character.
    pub fn try_new(
        destination: Destination,
        path: impl Into<String>,
        headers: Vec<(String, String)>,
    ) -> Result<Self, EndpointError> {
        let path = path.into();
        validate_path(&path)?;
        validate_headers(&headers)?;
        Ok(Self {
            destination,
            path,
            headers,
        })
    }

    /// Returns the peer to connect to; TLS policy and budgets stay with the data plane.
    #[must_use]
    pub const fn destination(&self) -> &Destination {
        &self.destination
    }

    /// Returns the absolute request path.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Returns the headers the provider adds to the request.
    #[must_use]
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }
}

/// One provider request exactly as the data plane will send it.
#[derive(Clone, PartialEq, Eq)]
pub struct WireRequest {
    endpoint: Endpoint,
    body: Vec<u8>,
}

impl fmt::Debug for WireRequest {
    /// Prints the body's length, never the body: it is the caller's prompt.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WireRequest")
            .field("endpoint", &self.endpoint)
            .field("body_len", &self.body.len())
            .finish()
    }
}

impl WireRequest {
    /// Pairs an endpoint with the body the adapter encoded.
    #[must_use]
    pub fn new(endpoint: Endpoint, body: Vec<u8>) -> Self {
        Self { endpoint, body }
    }

    /// Returns where the request goes.
    #[must_use]
    pub const fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Returns the encoded body.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
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

    /// Returns what this provider can express.
    fn capabilities(&self) -> Capabilities;

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

fn validate_path(path: &str) -> Result<(), EndpointError> {
    if !path.starts_with('/') {
        return Err(EndpointError::Path);
    }
    if path
        .bytes()
        .any(|byte| byte.is_ascii_control() || byte == b' ')
    {
        return Err(EndpointError::Path);
    }
    Ok(())
}

fn validate_headers(headers: &[(String, String)]) -> Result<(), EndpointError> {
    for (name, value) in headers {
        if !is_token(name) {
            return Err(EndpointError::HeaderName { name: name.clone() });
        }
        if value.bytes().any(|byte| byte.is_ascii_control()) {
            return Err(EndpointError::HeaderValue { name: name.clone() });
        }
    }
    Ok(())
}

/// Returns whether `name` is an HTTP token (RFC 9110 `tchar`), the only form a header name may take.
fn is_token(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}
