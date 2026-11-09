//! A client codec and a provider adapter with no vendor format, for the exchange tests.
//!
//! The wire format is deliberately trivial so the tests measure orchestration rather than parsing: a
//! request body is `model\ntext`, a response body is `id|text`, and one stream line is one text
//! delta. Two request facts opt into the features the capability gate checks — an `x-mock-stream`
//! header and an `image:` prefix — so a test can make a request need a capability the pair lacks.

use std::{collections::BTreeMap, sync::Arc};

use nico_core::ErrorCategory;
use nico_http::Destination;
use nico_llm::{
    AssistantTurn, CanonicalError, Capabilities, Capability, ChatRequest, ChatResponse, Choice,
    ClientCodec, ClientReply, ClientStreamEncoder, CodecError, ContentPart, Credential, Endpoint,
    Exchange, ExchangeMode, FinishReason, ImageSource, Item, Message, ModelId, ProtocolId,
    Provider, ProviderError, ProviderKind, ProviderRoute, ProviderStream, RequestFacts,
    ResponseFacts, ResponseId, Role, StreamEvent,
};

/// The protocol both mocks speak; a test that pairs a different one is testing a refusal.
pub(crate) const PROTOCOL: ProtocolId = ProtocolId::new("mock.chat");
/// The provider kind both mocks answer to.
pub(crate) const KIND: ProviderKind = ProviderKind::new("mock");
/// The status the mock protocol answers a complete response with.
///
/// A client protocol owns its success status, so the tests can answer `200` while the provider
/// answers `201` and assert which one the caller sees.
pub(crate) const SUCCESS_STATUS: u16 = 200;

/// How a mock stream behaves.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamBehaviour {
    /// Splits incoming bytes into lines and emits one text delta per line.
    Lines,
    /// Fails on the first chunk, as a provider whose body is not its own protocol would.
    FailOnPush,
}

/// Records the order in which a mock codec and provider are called.
#[derive(Clone, Default)]
pub(crate) struct Trace(std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>);

impl Trace {
    pub(crate) fn record(&self, step: &'static str) {
        self.0
            .lock()
            .expect("the trace mutex is never poisoned")
            .push(step);
    }

    pub(crate) fn steps(&self) -> Vec<&'static str> {
        self.0
            .lock()
            .expect("the trace mutex is never poisoned")
            .clone()
    }
}

/// Decodes `model\ntext` and encodes `id|text`, one line per stream event.
pub(crate) struct MockCodec {
    capabilities: Capabilities,
    trace: Option<Trace>,
}

impl MockCodec {
    pub(crate) fn new(capabilities: Capabilities) -> Arc<Self> {
        Arc::new(Self {
            capabilities,
            trace: None,
        })
    }

    /// Builds a codec that records when it decodes a request.
    pub(crate) fn traced(capabilities: Capabilities, trace: Trace) -> Arc<Self> {
        Arc::new(Self {
            capabilities,
            trace: Some(trace),
        })
    }
}

impl ClientCodec for MockCodec {
    fn protocol(&self) -> ProtocolId {
        PROTOCOL
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    fn decode_request(&self, facts: &RequestFacts, body: &[u8]) -> Result<ChatRequest, CodecError> {
        if let Some(trace) = &self.trace {
            trace.record("decode");
        }
        if facts.method() != "POST" {
            return Err(invalid("the method must be POST"));
        }
        let text = std::str::from_utf8(body).map_err(|_| invalid("the body is not UTF-8"))?;
        let (model, message) = text
            .split_once('\n')
            .ok_or_else(|| invalid("the body must be `model\\ntext`"))?;
        let model = ModelId::try_new(model).map_err(|error| invalid(&error.to_string()))?;
        let content = if message.starts_with("image:") {
            vec![ContentPart::Image(ImageSource::Url(
                "https://example.test/image.png".to_owned(),
            ))]
        } else {
            vec![ContentPart::text(message)]
        };
        let item = Item::Message(
            Message::try_new(Role::User, content).map_err(|error| invalid(&error.to_string()))?,
        );
        let request =
            ChatRequest::try_new(model, vec![item]).map_err(|error| invalid(&error.to_string()))?;
        if facts.header("x-mock-stream") == Some("1") {
            return Ok(request.with_stream(true));
        }
        Ok(request)
    }

    fn encode_response(&self, response: &ChatResponse) -> Result<ClientReply, CodecError> {
        Ok(ClientReply::new(
            SUCCESS_STATUS,
            format!("{}|{}", response.id(), answer(response)).into_bytes(),
        ))
    }

    fn stream_encoder(&self) -> Box<dyn ClientStreamEncoder> {
        Box::new(MockStreamEncoder)
    }

    fn encode_error(&self, error: &CanonicalError) -> Result<Vec<u8>, CodecError> {
        Ok(format!("error:{}:{}\n", error.http_status(), error.code()).into_bytes())
    }
}

/// Encodes one text delta per line and nothing for the other events.
struct MockStreamEncoder;

impl ClientStreamEncoder for MockStreamEncoder {
    fn push(&mut self, event: &StreamEvent) -> Result<Vec<u8>, CodecError> {
        let line = match event {
            StreamEvent::TextDelta(text) => format!("text:{text}\n"),
            StreamEvent::Finish(reason) => format!("finish:{reason:?}\n"),
            _ => {
                return Err(CodecError::Unsupported {
                    protocol: PROTOCOL,
                    feature: Capability::Streaming,
                });
            }
        };
        Ok(line.into_bytes())
    }

    fn finish(&mut self) -> Result<Vec<u8>, CodecError> {
        Ok(Vec::new())
    }
}

/// Builds the request body a test sends.
pub(crate) fn body(model: &str, text: &str) -> Vec<u8> {
    format!("{model}\n{text}").into_bytes()
}

/// Returns the facts of a mock request.
pub(crate) fn request_facts() -> RequestFacts {
    RequestFacts::new(
        "POST",
        "/v1/mock/chat",
        vec![("content-type".to_owned(), "text/plain".to_owned())],
    )
}

/// Returns the facts of a mock request that asks for a stream.
pub(crate) fn streaming_request_facts() -> RequestFacts {
    RequestFacts::new(
        "POST",
        "/v1/mock/chat",
        vec![("x-mock-stream".to_owned(), "1".to_owned())],
    )
}

/// Encodes `model\ntext` and decodes `id|text` as a provider that presents its route's credential.
pub(crate) struct MockProvider {
    capabilities: Capabilities,
    stream: StreamBehaviour,
    trace: Option<Trace>,
    /// The model this provider's endpoint path names, as an adapter whose request target carries the
    /// physical model does; `None` means the model travels in the body.
    pinned_model: Option<ModelId>,
    /// Whether failure statuses answer retryable, as a provider whose closed table calls transport
    /// failures (408, 429, 5xx) retryable does; a test of the attempt sequence needs both postures.
    retryable: bool,
}

impl MockProvider {
    pub(crate) fn new(capabilities: Capabilities, stream: StreamBehaviour) -> Arc<Self> {
        Arc::new(Self {
            capabilities,
            stream,
            trace: None,
            pinned_model: None,
            retryable: false,
        })
    }

    /// Builds a provider whose failure statuses answer retryable, the posture a transport-failure
    /// table has. The exchange tests never ask it (they have no attempt sequence); the route tests do.
    pub(crate) fn retrying(capabilities: Capabilities, stream: StreamBehaviour) -> Arc<Self> {
        Arc::new(Self {
            capabilities,
            stream,
            trace: None,
            pinned_model: None,
            retryable: true,
        })
    }

    /// Builds a provider that records its endpoint and encode calls.
    pub(crate) fn traced(
        capabilities: Capabilities,
        stream: StreamBehaviour,
        trace: Trace,
    ) -> Arc<Self> {
        Arc::new(Self {
            capabilities,
            stream,
            trace: Some(trace),
            pinned_model: None,
            retryable: false,
        })
    }

    /// Returns the same provider, whose endpoint path names `model`.
    ///
    /// The mock therefore answers the two questions an adapter with a model in its path answers: the
    /// endpoint carries a model, and the body still carries the one the route resolved.
    pub(crate) fn pinning(self: &Arc<Self>, model: ModelId) -> Arc<Self> {
        Arc::new(Self {
            capabilities: self.capabilities,
            stream: self.stream,
            trace: self.trace.clone(),
            pinned_model: Some(model),
            retryable: self.retryable,
        })
    }

    fn malformed(&self, reason: &str) -> ProviderError {
        ProviderError::Malformed {
            kind: KIND,
            reason: reason.to_owned(),
        }
    }
}

impl Provider for MockProvider {
    fn kind(&self) -> ProviderKind {
        KIND
    }

    fn native_protocol(&self) -> ProtocolId {
        PROTOCOL
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    fn endpoint(&self, route: &ProviderRoute) -> Result<Endpoint, ProviderError> {
        if let Some(trace) = &self.trace {
            trace.record("endpoint");
        }
        let authorization = route.credential().map_or_else(String::new, |credential| {
            format!("Bearer {}", credential.expose())
        });
        let endpoint = Endpoint::try_new(
            Destination::new(nico_core::Scheme::Http, "127.0.0.1", 8080),
            "/v1/mock",
            vec![("authorization".to_owned(), authorization)],
        )
        .map_err(|source| ProviderError::InvalidEndpoint { kind: KIND, source })?;
        Ok(match &self.pinned_model {
            Some(model) => endpoint.with_model(model.clone()),
            None => endpoint,
        })
    }

    fn encode(
        &self,
        route: &ProviderRoute,
        request: &ChatRequest,
    ) -> Result<Vec<u8>, ProviderError> {
        if let Some(trace) = &self.trace {
            trace.record("encode");
        }
        let model = route.model(request.model());
        let text = request
            .conversation()
            .iter()
            .find_map(|item| match item {
                Item::Message(message) => message.content().iter().find_map(|part| match part {
                    ContentPart::Text(text) => Some(text.clone()),
                    _ => None,
                }),
                _ => None,
            })
            .unwrap_or_default();
        Ok(format!("{}\n{text}\n", model.physical()).into_bytes())
    }

    fn decode_response(
        &self,
        head: &ResponseFacts,
        body: &[u8],
    ) -> Result<ChatResponse, ProviderError> {
        if !head.is_success() {
            let error = CanonicalError::try_new(
                ErrorCategory::Upstream,
                "mock_refused",
                "the mock provider refused the request",
                head.status(),
            )
            .map_err(|error| self.malformed(&error.to_string()))?;
            return Err(ProviderError::Upstream { error });
        }
        let text =
            std::str::from_utf8(body).map_err(|_| self.malformed("the body is not UTF-8"))?;
        let (id, answer) = text
            .split_once('|')
            .ok_or_else(|| self.malformed("the body must be `id|text`"))?;
        let turn = AssistantTurn::try_new(vec![ContentPart::text(answer)], Vec::new())
            .map_err(|error| self.malformed(&error.to_string()))?;
        let choice = Choice::try_new(0, turn, FinishReason::Stop)
            .map_err(|error| self.malformed(&error.to_string()))?;
        ChatResponse::try_new(
            ResponseId::try_new(id).map_err(|error| self.malformed(&error.to_string()))?,
            ModelId::try_new("mock-physical")
                .map_err(|error| self.malformed(&error.to_string()))?,
            vec![choice],
        )
        .map_err(|error| self.malformed(&error.to_string()))
    }

    fn stream_decoder(&self) -> Box<dyn ProviderStream> {
        Box::new(MockStream {
            buffer: Vec::new(),
            behaviour: self.stream,
        })
    }

    fn error_for_status(&self, status: u16) -> CanonicalError {
        CanonicalError::try_new(
            ErrorCategory::Upstream,
            "mock_refused",
            "the mock provider refused the request",
            status,
        )
        .expect("the mock status table is a constant")
        .with_retryable(self.retryable)
    }
}

/// Completes one line per chunk boundary and emits one text delta per line.
struct MockStream {
    buffer: Vec<u8>,
    behaviour: StreamBehaviour,
}

impl ProviderStream for MockStream {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<StreamEvent>, ProviderError> {
        if self.behaviour == StreamBehaviour::FailOnPush {
            return Err(ProviderError::Malformed {
                kind: KIND,
                reason: "the mock stream failed".to_owned(),
            });
        }
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(position) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line = self.buffer.drain(..=position).collect::<Vec<_>>();
            let text = String::from_utf8(line).map_err(|_| ProviderError::Malformed {
                kind: KIND,
                reason: "the stream line is not UTF-8".to_owned(),
            })?;
            events.push(StreamEvent::TextDelta(
                text.trim_end_matches('\n').to_owned(),
            ));
        }
        Ok(events)
    }

    fn finish(&mut self) -> Result<Vec<StreamEvent>, ProviderError> {
        if !self.buffer.is_empty() {
            return Err(ProviderError::Malformed {
                kind: KIND,
                reason: "the stream ended mid-line".to_owned(),
            });
        }
        Ok(vec![StreamEvent::Finish(FinishReason::Stop)])
    }
}

/// Builds a route for the mock protocol and kind, with one model mapping and a credential.
pub(crate) fn route(mode: ExchangeMode) -> Arc<ProviderRoute> {
    let mut model_map = BTreeMap::new();
    model_map.insert("chat-small".to_owned(), "chat-small-2026-01-01".to_owned());
    Arc::new(
        ProviderRoute::try_new("chat-route", PROTOCOL, KIND, "http://127.0.0.1:8080")
            .expect("a valid route")
            .with_mode(mode)
            .with_credential(Credential::new("sk-mock-secret"))
            .with_model_map(model_map)
            .expect("a valid model map"),
    )
}

/// Pairs mocks with the requested capabilities.
pub(crate) fn exchange(
    codec_capabilities: Capabilities,
    provider_capabilities: Capabilities,
) -> Exchange {
    Exchange::try_new(
        MockCodec::new(codec_capabilities),
        MockProvider::new(provider_capabilities, StreamBehaviour::Lines),
        route(ExchangeMode::Convert),
    )
    .expect("the mocks pair on the route")
}

fn invalid(reason: &str) -> CodecError {
    CodecError::InvalidRequest {
        protocol: PROTOCOL,
        reason: reason.to_owned(),
    }
}

fn answer(response: &ChatResponse) -> String {
    response
        .choices()
        .first()
        .and_then(|choice| choice.turn().content().first())
        .and_then(|part| match part {
            ContentPart::Text(text) => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}
