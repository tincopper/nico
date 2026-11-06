//! The exchange: one client protocol's request decoded, one provider's wire encoded, and the
//! response translated back.
//!
//! This module is the only place that knows both halves exist. It holds no framework type and no
//! vendor knowledge: a client codec and a provider adapter meet here as trait objects, and the
//! canonical model is the only value that crosses between them. The data plane therefore sees bytes
//! in and bytes out, which is exactly what keeps the HTTP crate free of this one.
//!
//! ```text
//! client bytes ──ClientCodec::decode_request──▶ ChatRequest ──Provider::encode──▶ WireRequest
//! client bytes ◀─ClientCodec::encode_response── ChatResponse ◀─Provider::decode_response── bytes
//! ```
//!
//! Every exchange failure leaves as an [`ExchangeError`], which renders itself as a canonical error;
//! a client codec turns that value into the client's own error envelope, so a provider's status code,
//! body and credential never reach the caller. Pairing a codec with an adapter is a publication
//! decision instead, and fails as an [`ExchangeBuildError`].

use std::{fmt, sync::Arc};

use crate::{
    protocol::{
        Capabilities, Capability, ClientCodec, ClientReply, CodecError, RequestFacts, ResponseFacts,
    },
    provider::{Endpoint, ExchangeMode, Provider, ProviderError, ProviderRoute, WireRequest},
    schema::{ChatRequest, ContentPart, Item, ModelRef, ResponseFormat},
};

mod error;
mod stream;

pub use error::{ExchangeBuildError, ExchangeError};
pub use stream::StreamEncoder;

/// One route's worth of exchange: the client codec, the provider adapter and the route that pairs
/// them.
///
/// Built once per route at publication (the codecs and adapters themselves are shared) and held by
/// the data plane as an `Arc`: the value is stateless, so a request clones a pointer and
/// [`Exchange::prepare`] produces the request's own canonical value and wire bytes. It holds the
/// capability intersection the publication computed, so a request that needs something the pair does
/// not share is refused here instead of failing inside a vendor encoder.
pub struct Exchange {
    codec: Arc<dyn ClientCodec>,
    provider: Arc<dyn Provider>,
    route: Arc<ProviderRoute>,
    capabilities: Capabilities,
}

impl Exchange {
    /// Pairs a codec, a provider adapter and the compiled route facts.
    ///
    /// # Errors
    /// Returns [`ExchangeBuildError::ProtocolMismatch`] when the codec's protocol is not the route's,
    /// [`ExchangeBuildError::ProviderMismatch`] when the adapter's kind is not the route's, and
    /// [`ExchangeBuildError::NotConverted`] for a passthrough route, which moves bytes without a
    /// canonical value and therefore has no exchange.
    pub fn try_new(
        codec: Arc<dyn ClientCodec>,
        provider: Arc<dyn Provider>,
        route: Arc<ProviderRoute>,
    ) -> Result<Self, ExchangeBuildError> {
        if codec.protocol() != route.protocol() {
            return Err(ExchangeBuildError::ProtocolMismatch {
                codec: codec.protocol(),
                route: route.protocol(),
            });
        }
        if provider.kind() != route.provider_kind() {
            return Err(ExchangeBuildError::ProviderMismatch {
                provider: provider.kind(),
                route: route.provider_kind(),
            });
        }
        if route.mode() != ExchangeMode::Convert {
            return Err(ExchangeBuildError::NotConverted {
                route_id: route.route_id().to_owned(),
            });
        }
        let capabilities = codec.capabilities().intersection(provider.capabilities());
        Ok(Self {
            codec,
            provider,
            route,
            capabilities,
        })
    }

    /// Returns the route this exchange serves.
    #[must_use]
    pub fn route(&self) -> &ProviderRoute {
        &self.route
    }

    /// Returns the capabilities both sides share, which the request path enforces.
    #[must_use]
    pub const fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    /// Resolves the upstream endpoint the route names.
    ///
    /// The result depends on the route facts only, so the data plane may ask before the request body
    /// arrives and send the upstream head early.
    ///
    /// # Errors
    /// Returns the adapter's error when the route's base URL or credential cannot form an endpoint.
    pub fn endpoint(&self) -> Result<Endpoint, ProviderError> {
        self.provider.endpoint(&self.route)
    }

    /// Decodes the buffered client body once and encodes it for the provider.
    ///
    /// The decode result is the canonical request the exchange returns; it is a value, so a retry or
    /// a model change re-encodes it instead of replaying the client's bytes.
    ///
    /// # Errors
    /// Returns [`ExchangeError::Codec`] when the client codec rejects the body,
    /// [`ExchangeError::InvalidCanonical`] when the decoded value breaks a canonical invariant,
    /// [`ExchangeError::Unsupported`] when the request needs a capability the pair does not share,
    /// and [`ExchangeError::Provider`] when the adapter cannot build its endpoint or encode the
    /// request.
    pub fn prepare(
        &self,
        facts: &RequestFacts,
        body: &[u8],
    ) -> Result<PreparedExchange, ExchangeError> {
        let endpoint = self.provider.endpoint(&self.route)?;
        let request = self.codec.decode_request(facts, body)?;
        request
            .validate()
            .map_err(|source| ExchangeError::InvalidCanonical { source })?;
        let needed = required(&request);
        if let Some(capability) = needed
            .iter()
            .find(|capability| !self.capabilities.supports(*capability))
        {
            return Err(ExchangeError::Unsupported { capability });
        }
        let model = self.route.model(request.model());
        let body = self.provider.encode(&self.route, &request)?;
        Ok(PreparedExchange {
            request,
            model,
            wire: WireRequest::new(endpoint, body),
        })
    }

    /// Decodes the buffered provider body and encodes it as the client's answer.
    ///
    /// A provider failure status is answered in the client's protocol: the adapter's
    /// [`ProviderError::Upstream`] carries the canonical error, which is encoded as an error reply
    /// with its own status. Every other adapter failure is returned, and the caller answers with
    /// [`Exchange::error_reply`]. A successful body is answered with the status the client codec
    /// chose, never the provider's: the exchange re-encodes the body, so the provider's framing
    /// carries no meaning for the caller.
    ///
    /// # Errors
    /// Returns [`ExchangeError::Provider`] when the body is not a document of the provider's protocol
    /// and [`ExchangeError::Codec`] when the client codec cannot encode the reply.
    pub fn respond(
        &self,
        facts: &ResponseFacts,
        body: &[u8],
    ) -> Result<ClientReply, ExchangeError> {
        let response = match self.provider.decode_response(facts, body) {
            Ok(response) => response,
            Err(ProviderError::Upstream { error }) => {
                let body = self.codec.encode_error(&error)?;
                return Ok(ClientReply::new(error.http_status(), body));
            }
            Err(other) => return Err(other.into()),
        };
        response
            .validate()
            .map_err(|source| ExchangeError::InvalidCanonical { source })?;
        Ok(self.codec.encode_response(&response)?)
    }

    /// Renders a failed exchange as the client protocol's error envelope.
    ///
    /// # Errors
    /// Returns the codec's error when it cannot build its own error envelope, which the data plane
    /// answers with a generic failure.
    pub fn error_reply(&self, error: &ExchangeError) -> Result<ClientReply, CodecError> {
        let canonical = error.canonical();
        let body = self.codec.encode_error(&canonical)?;
        Ok(ClientReply::new(canonical.http_status(), body))
    }

    /// Creates the per-response stream encoder.
    ///
    /// The provider decoder is created fresh here, so a partial frame from one response can never be
    /// completed by another one's bytes.
    #[must_use]
    pub fn stream(&self) -> StreamEncoder {
        StreamEncoder::new(Arc::clone(&self.codec), self.provider.stream_decoder())
    }
}

impl std::fmt::Debug for Exchange {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Exchange")
            .field("route_id", &self.route.route_id())
            .field("protocol", &self.route.protocol())
            .field("provider_kind", &self.route.provider_kind())
            .field("capabilities", &self.capabilities)
            .finish()
    }
}

/// One prepared request: the canonical value, the model pair and the bytes to send upstream.
pub struct PreparedExchange {
    request: ChatRequest,
    model: ModelRef,
    wire: WireRequest,
}

impl fmt::Debug for PreparedExchange {
    /// Summarizes the request and never prints it: the conversation is the caller's prompt.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedExchange")
            .field("model", &self.model)
            .field("stream", &self.request.stream())
            .field("items", &self.request.conversation().len())
            .field("wire", &self.wire)
            .finish()
    }
}

impl PreparedExchange {
    /// Returns the decoded canonical request, which a retry may re-encode.
    #[must_use]
    pub fn request(&self) -> &ChatRequest {
        &self.request
    }

    /// Returns the logical and physical model names, for the request record.
    #[must_use]
    pub fn model(&self) -> &ModelRef {
        &self.model
    }

    /// Returns the endpoint and body the data plane sends.
    #[must_use]
    pub fn wire(&self) -> &WireRequest {
        &self.wire
    }
}

/// Returns the capabilities a request needs beyond plain text.
///
/// The walk is over the canonical model only, so it stays vendor-free: a request needs tools if it
/// declares them or answers one, images or files if a part carries one, and reasoning if the
/// conversation already holds one. Plain text is not a capability; it is what a chat protocol does.
fn required(request: &ChatRequest) -> Capabilities {
    let mut needed = Capabilities::none();
    if request.stream() {
        needed = needed.with(Capability::Streaming);
    }
    if !request.tools().is_empty() {
        needed = needed.with(Capability::Tools);
    }
    if !matches!(request.response_format(), ResponseFormat::Text) {
        needed = needed.with(Capability::ResponseFormat);
    }
    for item in request.conversation() {
        needed = match item {
            Item::Message(message) => with_parts(needed, message.content()),
            Item::ToolResult(result) => {
                with_parts(needed.with(Capability::Tools), result.content())
            }
            Item::ToolCall(_) => needed.with(Capability::Tools),
            Item::Reasoning(_) => needed.with(Capability::Reasoning),
        };
    }
    needed
}

/// Adds the capabilities the content parts of one item need.
fn with_parts(mut needed: Capabilities, content: &[ContentPart]) -> Capabilities {
    for part in content {
        needed = match part {
            ContentPart::Text(_) => needed,
            ContentPart::Image(_) => needed.with(Capability::Images),
            ContentPart::File { .. } => needed.with(Capability::Files),
        };
    }
    needed
}
