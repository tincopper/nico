//! Shared fixtures for the front-end tests.
//!
//! The tests pair the real Chat Completions codec with the real adapter: what is asserted is the
//! front-end's translation of a document it would see in production, not a mock's idea of one. Two
//! fixtures are mocks on purpose — a codec that cannot encode its own error document and a provider
//! that answers success to a failure status — because each exercises a fail-closed branch the real
//! pair cannot reach.

#![expect(
    dead_code,
    reason = "each test file uses the subset of these helpers its scenarios need"
)]

use std::sync::Arc;

use nico_http::llm::{ConvertingRoute, LlmRequest};
use nico_llm::{
    AssistantTurn, AttemptChain, BreakerParams, Capabilities, ChainMember, ChatRequest,
    ChatResponse, Choice, ClientCodec, ClientReply, ClientStreamEncoder, CodecError, ContentPart,
    Credential, Exchange, FinishReason, ModelId, Provider, ProviderError, ProviderRoute,
    ProviderStream, ResponseFacts, ResponseId, StreamEvent, TargetIdentity,
    protocol::openai_chat::{OpenAiChatCodec, PROTOCOL},
    provider::openai::{OpenAiProvider, PROVIDER_KIND},
    route::{ConvertRoute, PassthroughRoute},
};

/// The base URL most tests use; a name and port only, so its path rules are the default ones.
pub(crate) const BASE_URL: &str = "http://127.0.0.1:8080";

/// The logical model a fixture request asks for.
pub(crate) const MODEL: &str = "chat-small";

/// An event stream large enough that no fixture meets it by accident.
pub(crate) const LIMIT: u32 = 1 << 20;

/// The adapter under test.
pub(crate) fn provider() -> OpenAiProvider {
    OpenAiProvider::new()
}

/// Builds a converting route for one base URL that presents no credential.
pub(crate) fn route(base_url: &str) -> ProviderRoute {
    ProviderRoute::try_new("chat", PROTOCOL, PROVIDER_KIND, base_url).expect("a valid route")
}

/// Builds a converting route with a credential and a logical-to-physical model map.
pub(crate) fn route_with(credential: Option<&str>, model_map: &[(&str, &str)]) -> ProviderRoute {
    let mut route = route(BASE_URL);
    if let Some(secret) = credential {
        route = route.with_credential(Credential::new(secret));
    }
    if !model_map.is_empty() {
        let map = model_map
            .iter()
            .map(|(logical, physical)| ((*logical).to_owned(), (*physical).to_owned()))
            .collect();
        route = route.with_model_map(map).expect("a valid model map");
    }
    route
}

/// Pairs the real codec and adapter with the default route.
pub(crate) fn exchange() -> Arc<Exchange> {
    exchange_with(Arc::new(OpenAiChatCodec::new()), Arc::new(provider()))
}

/// Pairs one codec and one adapter with the default route.
pub(crate) fn exchange_with(
    codec: Arc<dyn ClientCodec>,
    provider: Arc<dyn Provider>,
) -> Arc<Exchange> {
    Arc::new(
        Exchange::try_new(
            codec,
            provider,
            Arc::new(route_with(
                Some("sk-test-secret"),
                &[(MODEL, "chat-small-2026-01-01")],
            )),
        )
        .expect("the codec, the adapter and the route pair"),
    )
}

/// Wraps one exchange into the single-target attempt chain a route without fallback publishes.
///
/// The identity is a fixture's: a named primary at the test base URL with the built-in breaker
/// defaults, equal for every test because no test reads it — the chain's construction, not its
/// state, is what these front-end tests exercise.
pub(crate) fn chain_of(exchange: Arc<Exchange>) -> Arc<AttemptChain> {
    let identity = TargetIdentity::new(
        "openai-primary",
        BASE_URL,
        PROVIDER_KIND,
        BreakerParams::new(5, 30_000),
    );
    Arc::new(
        AttemptChain::try_new(vec![ChainMember::new(exchange, identity)])
            .expect("one member is a chain"),
    )
}

/// The breaker state one fixture route's walk consults.
///
/// A fresh table per fixture: no test reads another's target history.
pub(crate) fn breakers() -> std::sync::Arc<nico_llm::BreakerTable> {
    std::sync::Arc::new(nico_llm::BreakerTable::new())
}

/// The converting front-end the data plane would publish for the default route.
///
/// Budget `1`: one attempt, the document's default, which is what a route that names no
/// `retry_budget` gets.
pub(crate) fn convert_route() -> ConvertRoute {
    ConvertRoute::new(chain_of(exchange()), breakers(), 1)
}

/// The passthrough front-end for a route in passthrough mode.
pub(crate) fn passthrough_route() -> PassthroughRoute {
    PassthroughRoute::try_new(
        Arc::new(route(BASE_URL).with_mode(nico_llm::ExchangeMode::Passthrough)),
        &provider(),
    )
    .expect("the route and the adapter agree")
}

/// The view the data plane hands a front-end for one request body.
pub(crate) fn view(body: &[u8]) -> LlmRequest<'_> {
    LlmRequest::new(
        "POST",
        "/v1/chat/completions",
        vec![("content-type", "application/json")],
        body,
    )
}

/// A client request body, streamed or complete.
pub(crate) fn client_body(stream: bool) -> Vec<u8> {
    let stream = if stream { r#","stream":true"# } else { "" };
    format!(
        r#"{{"model":"{MODEL}","messages":[{{"role":"user","content":"PROMPT-MARKER"}}]{stream}}}"#
    )
    .into_bytes()
}

/// A provider completion document for the default route.
pub(crate) fn completion(text: &str) -> Vec<u8> {
    format!(
        r#"{{"id":"chatcmpl-1","object":"chat.completion","created":1,"model":"chat-small-2026-01-01","choices":[{{"index":0,"message":{{"role":"assistant","content":"{text}"}},"finish_reason":"stop"}}]}}"#
    )
    .into_bytes()
}

/// A provider completion document that reports token usage.
pub(crate) fn completion_with_usage(text: &str, prompt: u64, completion: u64) -> Vec<u8> {
    format!(
        r#"{{"id":"chatcmpl-1","object":"chat.completion","created":1,"model":"chat-small-2026-01-01","choices":[{{"index":0,"message":{{"role":"assistant","content":"{text}"}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":{prompt},"completion_tokens":{completion},"total_tokens":{total}}}}}"#,
        total = prompt + completion
    )
    .into_bytes()
}

/// Wraps one provider event in an SSE frame.
pub(crate) fn frame(data: &str) -> String {
    format!("data: {data}\n\n")
}

/// The provider event stream for a two-token answer (the second one multi-byte), terminator
/// included.
pub(crate) fn provider_stream() -> Vec<u8> {
    let chunks = [
        frame(
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"chat-small-2026-01-01","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
        ),
        frame(r#"{"choices":[{"index":0,"delta":{"content":"Hi"},"finish_reason":null}]}"#),
        frame(r#"{"choices":[{"index":0,"delta":{"content":" ✓"},"finish_reason":null}]}"#),
        frame(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#),
        frame("[DONE]"),
    ];
    chunks.concat().into_bytes()
}

/// Drives one buffered response through a fresh transform and returns its bytes.
pub(crate) fn buffered(
    route: &ConvertRoute,
    status: u16,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<(u16, Vec<String>, Vec<u8>), nico_http::error::HttpError> {
    let mut transform = route.transform(false, LIMIT).expect("a transform is built");
    let head = transform.head(status, &owned(headers))?;
    transform.push(body)?;
    let body = transform.finish()?;
    Ok((
        head.status(),
        head.headers()
            .iter()
            .map(|(name, value)| format!("{name}: {value}"))
            .collect(),
        body,
    ))
}

/// Drives one event stream through a fresh transform, feeding the provider bytes in fixed slices.
pub(crate) fn streamed(
    route: &ConvertRoute,
    provider: &[u8],
    size: usize,
) -> Result<Vec<u8>, nico_http::error::HttpError> {
    let mut transform = route.transform(true, LIMIT).expect("a transform is built");
    let head = transform.head(200, &owned(&[("content-type", "text/event-stream")]))?;
    assert_eq!(head.status(), 200);
    let mut bytes = Vec::new();
    for chunk in provider.chunks(size) {
        bytes.extend(transform.push(chunk)?);
    }
    bytes.extend(transform.finish()?);
    Ok(bytes)
}

/// Owns a header list, the shape the data plane hands a transform.
pub(crate) fn owned(headers: &[(&str, &str)]) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

/// A codec that cannot read a request and cannot render its own failure.
pub(crate) struct FailingCodec;

impl ClientCodec for FailingCodec {
    fn protocol(&self) -> nico_llm::ProtocolId {
        PROTOCOL
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::none()
    }

    fn decode_request(
        &self,
        _facts: &nico_llm::RequestFacts,
        _body: &[u8],
    ) -> Result<ChatRequest, CodecError> {
        Err(CodecError::InvalidRequest {
            protocol: PROTOCOL,
            reason: "the fixture codec refuses every document".to_owned(),
        })
    }

    fn encode_response(&self, _response: &ChatResponse) -> Result<ClientReply, CodecError> {
        Err(self.encode_failure())
    }

    fn stream_encoder(&self) -> Box<dyn ClientStreamEncoder> {
        Box::new(FailingStream)
    }

    fn encode_error(&self, _error: &nico_llm::CanonicalError) -> Result<Vec<u8>, CodecError> {
        Err(self.encode_failure())
    }
}

impl FailingCodec {
    fn encode_failure(&self) -> CodecError {
        CodecError::Encode {
            protocol: PROTOCOL,
            reason: "the fixture codec cannot encode".to_owned(),
        }
    }
}

/// A stream encoder that refuses its first event.
struct FailingStream;

impl ClientStreamEncoder for FailingStream {
    fn push(&mut self, _event: &StreamEvent) -> Result<Vec<u8>, CodecError> {
        Err(CodecError::Encode {
            protocol: PROTOCOL,
            reason: "the fixture codec cannot encode".to_owned(),
        })
    }

    fn finish(&mut self) -> Result<Vec<u8>, CodecError> {
        Err(CodecError::Encode {
            protocol: PROTOCOL,
            reason: "the fixture codec cannot encode".to_owned(),
        })
    }
}

/// A provider that delegates everything to the real adapter but answers success to any status.
pub(crate) struct SuccessOnFailure {
    inner: OpenAiProvider,
}

impl SuccessOnFailure {
    /// Wraps the real adapter.
    pub(crate) const fn new() -> Self {
        Self {
            inner: OpenAiProvider::new(),
        }
    }
}

impl Provider for SuccessOnFailure {
    fn kind(&self) -> nico_llm::ProviderKind {
        self.inner.kind()
    }

    fn native_protocol(&self) -> nico_llm::ProtocolId {
        self.inner.native_protocol()
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn endpoint(&self, route: &ProviderRoute) -> Result<nico_llm::Endpoint, ProviderError> {
        self.inner.endpoint(route)
    }

    fn encode(
        &self,
        route: &ProviderRoute,
        request: &ChatRequest,
    ) -> Result<Vec<u8>, ProviderError> {
        self.inner.encode(route, request)
    }

    fn decode_response(
        &self,
        _head: &ResponseFacts,
        body: &[u8],
    ) -> Result<ChatResponse, ProviderError> {
        // The body is a fixture success document; the status is ignored on purpose, which is the
        // one thing the real adapter never does.
        serde_json::from_slice(body).map_err(|error| ProviderError::Malformed {
            kind: PROVIDER_KIND,
            reason: error.to_string(),
        })
    }

    fn error_for_status(&self, status: u16) -> nico_llm::CanonicalError {
        self.inner.error_for_status(status)
    }

    fn stream_decoder(&self) -> Box<dyn ProviderStream> {
        self.inner.stream_decoder()
    }
}

/// The canonical success document [`SuccessOnFailure`] answers with.
pub(crate) fn success_document() -> Vec<u8> {
    let response = ChatResponse::try_new(
        ResponseId::try_new("resp-1").expect("a valid id"),
        ModelId::try_new("chat-small-2026-01-01").expect("a valid model"),
        vec![
            Choice::try_new(
                0,
                AssistantTurn::try_new(vec![ContentPart::text("hello")], Vec::new())
                    .expect("a valid turn"),
                FinishReason::Stop,
            )
            .expect("a valid choice"),
        ],
    )
    .expect("a valid response");
    serde_json::to_vec(&response).expect("a canonical response serializes")
}
