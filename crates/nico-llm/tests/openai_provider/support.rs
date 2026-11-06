//! Shared fixtures for the OpenAI provider adapter tests.

#![expect(
    dead_code,
    reason = "each test file uses the subset of these helpers its scenarios need"
)]

use nico_core::Scheme;
use nico_llm::{
    ChatRequest, ChatResponse, ContentPart, Credential, Endpoint, ImageSource, Item, Message,
    ModelId, Provider, ProviderError, ProviderRoute, RawJson, Reasoning, ResponseFacts, Role,
    StreamEvent, Tool, ToolCall, ToolCallId, ToolName, ToolResult,
    protocol::openai_chat::{OpenAiChatCodec, PROTOCOL},
    provider::openai::{OpenAiProvider, PROVIDER_KIND},
};
use serde_json::Value;

/// The base URL most tests use; a name and port only, so its path rules are the default ones.
pub(crate) const BASE_URL: &str = "http://127.0.0.1:8080";

/// The adapter under test.
pub(crate) fn provider() -> OpenAiProvider {
    OpenAiProvider::new()
}

/// Builds a route for one base URL that converts and presents no credential.
pub(crate) fn route(base_url: &str) -> ProviderRoute {
    ProviderRoute::try_new("chat", PROTOCOL, PROVIDER_KIND, base_url).expect("a valid route")
}

/// Builds a route with a credential and a logical-to-physical model map.
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

/// Resolves the route's endpoint.
pub(crate) fn endpoint(route: &ProviderRoute) -> Endpoint {
    provider()
        .endpoint(route)
        .expect("the route addresses an endpoint")
}

/// Returns the first value of a header, compared ASCII-case-insensitively.
pub(crate) fn header<'a>(endpoint: &'a Endpoint, name: &str) -> Option<&'a str> {
    endpoint
        .headers()
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// The logical model a fixture request asks for.
pub(crate) fn model() -> ModelId {
    ModelId::try_new("chat-small").expect("a valid model")
}

/// Builds a request from its conversation items.
pub(crate) fn request(items: Vec<Item>) -> ChatRequest {
    ChatRequest::try_new(model(), items).expect("a valid request")
}

/// Builds a one-message user request.
pub(crate) fn user_request(text: &str) -> ChatRequest {
    request(vec![Item::Message(Message::user(text))])
}

/// Wraps a message item.
pub(crate) fn message(role: Role, parts: Vec<ContentPart>) -> Item {
    Item::Message(Message::try_new(role, parts).expect("a valid message"))
}

/// An assistant message with one text part.
pub(crate) fn assistant(text: &str) -> Item {
    message(Role::Assistant, vec![ContentPart::text(text)])
}

/// A reasoning item with no provider signature.
pub(crate) fn reasoning(text: &str) -> Item {
    Item::Reasoning(Reasoning::try_new(text, None).expect("valid reasoning"))
}

/// A tool call whose arguments are the given JSON document.
pub(crate) fn call(id: &str, name: &str, arguments: Value) -> Item {
    Item::ToolCall(
        ToolCall::try_new(
            ToolCallId::try_new(id).expect("a valid call id"),
            ToolName::try_new(name).expect("a valid tool name"),
            RawJson::new(arguments),
        )
        .expect("valid arguments"),
    )
}

/// A tool result carrying one text part.
pub(crate) fn result(id: &str, text: &str) -> Item {
    Item::ToolResult(
        ToolResult::try_new(
            ToolCallId::try_new(id).expect("a valid call id"),
            vec![ContentPart::text(text)],
        )
        .expect("a valid result"),
    )
}

/// A function tool with an empty object schema.
pub(crate) fn tool(name: &str) -> Tool {
    Tool::try_new(
        ToolName::try_new(name).expect("a valid tool name"),
        RawJson::new(serde_json::json!({"type": "object"})),
    )
    .expect("a valid tool")
}

/// An inline image part.
pub(crate) fn inline_image(media_type: &str, data: &[u8]) -> ContentPart {
    ContentPart::Image(ImageSource::Base64 {
        media_type: media_type.to_owned(),
        data: data.to_vec(),
    })
}

/// A remote image part.
pub(crate) fn remote_image(url: &str) -> ContentPart {
    ContentPart::Image(ImageSource::Url(url.to_owned()))
}

/// Encodes a request through the default route and parses the document.
pub(crate) fn encoded(request: &ChatRequest) -> Value {
    encoded_on(&route(BASE_URL), request)
}

/// Encodes a request through one route and parses the document.
pub(crate) fn encoded_on(route: &ProviderRoute, request: &ChatRequest) -> Value {
    let body = provider()
        .encode(route, request)
        .expect("the request encodes");
    serde_json::from_slice(&body).expect("an encoded request is JSON")
}

/// Encodes a request that must be refused.
pub(crate) fn encode_err(request: &ChatRequest) -> ProviderError {
    provider()
        .encode(&route(BASE_URL), request)
        .expect_err("the request must be refused")
}

/// Facts of a provider response with the given status.
pub(crate) fn facts(status: u16) -> ResponseFacts {
    ResponseFacts::new(
        status,
        vec![("content-type".to_owned(), "application/json".to_owned())],
    )
}

/// Decodes a provider response body that must be accepted.
pub(crate) fn decoded(body: &str) -> ChatResponse {
    provider()
        .decode_response(&facts(200), body.as_bytes())
        .expect("the body is a readable response")
}

/// Decodes a provider response that must be refused.
pub(crate) fn decode_err(status: u16, body: &str) -> ProviderError {
    provider()
        .decode_response(&facts(status), body.as_bytes())
        .expect_err("the response must be refused")
}

/// Builds one SSE frame carrying `data`.
pub(crate) fn frame(data: &str) -> String {
    format!("data: {data}\n\n")
}

/// Builds the terminator frame.
pub(crate) fn done() -> String {
    frame("[DONE]")
}

/// Feeds every frame to one decoder and returns the events.
pub(crate) fn events(chunks: &[String]) -> Vec<StreamEvent> {
    let mut decoder = provider().stream_decoder();
    let mut events = Vec::new();
    for chunk in chunks {
        events.extend(
            decoder
                .push(chunk.as_bytes())
                .expect("the stream is readable"),
        );
    }
    events.extend(decoder.finish().expect("the stream terminates completely"));
    events
}

/// Feeds the frames with every possible chunk size and returns the events of one size.
pub(crate) fn events_at(chunks: &[String], size: usize) -> Vec<StreamEvent> {
    let joined = chunks.concat();
    let mut decoder = provider().stream_decoder();
    let mut events = Vec::new();
    for part in joined.as_bytes().chunks(size) {
        events.extend(decoder.push(part).expect("the stream is readable"));
    }
    events.extend(decoder.finish().expect("the stream terminates completely"));
    events
}

/// Feeds the frames until one push fails and returns that failure.
pub(crate) fn push_err(chunks: &[String]) -> ProviderError {
    let mut decoder = provider().stream_decoder();
    let mut failure = None;
    for chunk in chunks {
        if let Err(error) = decoder.push(chunk.as_bytes()) {
            failure = Some(error);
            break;
        }
    }
    failure.expect("the stream must fail while pushing a chunk")
}

/// Feeds every frame and returns the failure the end of the body reports.
pub(crate) fn finish_err(chunks: &[String]) -> ProviderError {
    let mut decoder = provider().stream_decoder();
    for chunk in chunks {
        decoder
            .push(chunk.as_bytes())
            .expect("the frames before the end are readable");
    }
    decoder
        .finish()
        .expect_err("the stream must be refused at its end")
}

/// Builds the facts of a client request the exchange hands to a codec.
pub(crate) fn request_facts() -> nico_llm::RequestFacts {
    nico_llm::RequestFacts::new(
        "POST",
        "/v1/chat/completions",
        vec![("content-type".to_owned(), "application/json".to_owned())],
    )
}

/// The client codec that pairs with this adapter in the exchange tests.
pub(crate) fn codec() -> OpenAiChatCodec {
    OpenAiChatCodec::new()
}

/// Returns the default port of a scheme, for endpoint assertions.
pub(crate) fn default_port(scheme: Scheme) -> u16 {
    match scheme {
        Scheme::Http => 80,
        _ => 443,
    }
}
