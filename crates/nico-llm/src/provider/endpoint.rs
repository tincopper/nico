//! Where one upstream request goes and what it carries: the endpoint and the wire request.
//!
//! A provider adapter answers with an [`Endpoint`] — destination, path and headers — and an encoded
//! body. This module pairs them into the [`WireRequest`] the data plane sends, and it owns the checks
//! that keep either part from becoming an ambiguous message on the wire.
//!
//! The method is deliberately absent: the data plane writes the request line, and the verb is the one
//! the client used — a chat request is a `POST` by the client protocol's own rules, and a passthrough
//! route must forward the bytes it was given. The same ownership covers `host`, `content-length`,
//! `transfer-encoding` and every hop-by-hop header, so an adapter that names one of those here does
//! not get to overrule the framing.
//!
//! Everything in this file is a value the data plane reads; nothing here opens a connection or knows
//! a protocol's semantics.

use std::{borrow::Cow, fmt};

use nico_http::Destination;

use crate::{protocol::header_names, schema::ModelId};

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
    /// The route facts do not answer a question this adapter needs before it can produce bytes.
    ///
    /// Some adapters can only address an endpoint when the route decides something the canonical
    /// request does not carry: the physical model a path names, or the output ceiling the dialect
    /// requires when the caller names none. The endpoint is derived from route facts alone, and the
    /// upstream head goes out before the request body is read, so a missing fact cannot be filled in
    /// later — the route is refused instead.
    ///
    /// This is therefore a **publication** failure: the document is refused and the previously
    /// published generation keeps serving. `field` is the dotted path of the configuration field, and
    /// `reason` describes the rule that broke, never the value it held: a route fact can name an
    /// endpoint's model or credential, and a diagnostic must not quote either.
    #[error("`{field}` does not answer a route fact this provider needs: {reason}")]
    RouteFact {
        /// Dotted path of the configuration field, for example `providers[].max_output_tokens`.
        field: &'static str,
        /// The rule the facts broke, as a diagnostic.
        reason: Cow<'static, str>,
    },
}

/// Where one upstream request goes and what it presents: destination, path and headers.
///
/// The data plane owns the method and the framing, so an endpoint carries only what the provider
/// decides: the verb it writes is the caller's (a chat request is a `POST` by the client protocol's
/// own rules), because a second opinion here could only disagree with the request line the body was
/// decoded from. That ownership is also why a provider must not set `host`, `content-length`,
/// `transfer-encoding` or a hop-by-hop header here: the data plane derives them from the destination
/// and the body it actually writes. The path is absolute and is derived from route facts, so the
/// data plane can send the head before the request body is available.
#[derive(Clone, PartialEq, Eq)]
pub struct Endpoint {
    destination: Destination,
    path: String,
    headers: Vec<(String, String)>,
    model: Option<ModelId>,
}

impl fmt::Debug for Endpoint {
    /// Names the headers and never prints their values: one of them is the provider credential.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Endpoint")
            .field("destination", &self.destination)
            .field("path", &self.path)
            .field("headers", &header_names(&self.headers))
            .field("model", &self.model)
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
            model: None,
        })
    }

    /// Records the physical model this endpoint's path already names.
    ///
    /// A provider whose request target carries the model (`/models/{model}:…`) cannot take it from
    /// the request body: the path is a publication-time value and the upstream head is written before
    /// the body is decoded. Such an adapter writes the pinned model into the path and records it here,
    /// so the exchange can refuse a request that resolved to a different model instead of silently
    /// sending the caller's bytes to a model the route did not choose.
    ///
    /// [`Endpoint::model`] therefore answers one question for every adapter: does the path already
    /// name a model? `None` — the default — means the model travels in the body and no pinning applies.
    #[must_use]
    pub fn with_model(mut self, model: ModelId) -> Self {
        self.model = Some(model);
        self
    }

    /// Returns the model this endpoint's path already names; `None` means the model goes in the body.
    #[must_use]
    pub const fn model(&self) -> Option<&ModelId> {
        self.model.as_ref()
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

    /// Consumes the request and returns the encoded body.
    ///
    /// The front-end hands these bytes to the data plane, so taking them avoids one more copy of a
    /// body that may be megabytes long.
    #[must_use]
    pub fn into_body(self) -> Vec<u8> {
        self.body
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
