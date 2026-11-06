//! The adapter inside the exchange: endpoint, prepare, respond and the stream halves together.

use std::sync::Arc;

use nico_llm::{
    Exchange, ExchangeBuildError, ExchangeError, Provider, ProviderError, RequestFacts,
    ResponseFacts, provider::openai::OpenAiProvider,
};
use serde_json::{Value, json};

use crate::support::{codec, frame, provider, request_facts, route, route_with};

/// Builds the exchange the data plane would hold for one route.
fn exchange() -> Exchange {
    exchange_on(Some("sk-test-secret"))
}

/// Builds the exchange for a route with an optional credential and a model map.
fn exchange_on(credential: Option<&str>) -> Exchange {
    Exchange::try_new(
        Arc::new(codec()),
        Arc::new(provider()),
        Arc::new(route_with(
            credential,
            &[("chat-small", "chat-small-2026-01-01")],
        )),
    )
    .expect("the codec, the adapter and the route pair")
}

/// The client request body the exchange decodes.
fn client_body() -> Vec<u8> {
    br#"{"model":"chat-small","messages":[{"role":"user","content":"PROMPT-MARKER"}]}"#.to_vec()
}

/// The facts of the client request.
fn client_facts() -> RequestFacts {
    request_facts()
}

/// The facts of a provider response with one status.
fn provider_facts(status: u16) -> ResponseFacts {
    ResponseFacts::new(status, Vec::new())
}

/// Parses a JSON body.
fn document(body: &[u8]) -> Value {
    serde_json::from_slice(body).expect("the body is JSON")
}

#[test]
fn a_route_whose_base_url_is_not_addressable_is_refused_at_publication() {
    // The endpoint depends on the route facts only, so an unaddressable base URL is a publication
    // failure that keeps the last known good document serving — not a 500 on every request of the
    // route. Pairing is where it is discovered, so it cannot be forgotten by a caller.
    let error = Exchange::try_new(
        Arc::new(codec()),
        Arc::new(provider()),
        Arc::new(route("ftp://api.example.test")),
    )
    .expect_err("the adapter cannot address this route");
    assert!(
        matches!(
            error,
            ExchangeBuildError::Endpoint {
                source: ProviderError::InvalidEndpoint { .. },
            }
        ),
        "{error:?}"
    );
}

#[test]
fn the_endpoint_is_available_before_the_body_is_decoded() {
    let exchange = exchange();
    let endpoint = exchange.endpoint();
    assert_eq!(endpoint.path(), "/v1/chat/completions");
    assert_eq!(
        endpoint
            .headers()
            .iter()
            .find(|(name, _)| name == "authorization")
            .map(|(_, value)| value.as_str()),
        Some("Bearer sk-test-secret")
    );
}

#[test]
fn prepare_writes_the_physical_model_and_the_client_body_once() {
    let prepared = exchange()
        .prepare(&client_facts(), &client_body())
        .expect("the request exchanges");
    assert_eq!(prepared.model().logical().as_str(), "chat-small");
    assert_eq!(
        prepared.model().physical().as_str(),
        "chat-small-2026-01-01"
    );
    assert!(!prepared.request().stream());
    assert_eq!(
        prepared.wire().endpoint().path(),
        "/v1/chat/completions",
        "the endpoint is the provider's, not the client's"
    );
    let body = document(prepared.wire().body());
    assert_eq!(body["model"], "chat-small-2026-01-01");
    assert_eq!(body["messages"][0]["content"], "PROMPT-MARKER");
}

#[test]
fn a_prepared_request_debug_names_nothing_from_the_prompt_or_the_credential() {
    let prepared = exchange()
        .prepare(&client_facts(), &client_body())
        .expect("the request exchanges");
    let record = format!("{prepared:?}");
    assert!(
        !record.contains("PROMPT-MARKER"),
        "the conversation is the caller's prompt: {record}"
    );
    assert!(
        !record.contains("sk-test-secret"),
        "the credential must not be printed: {record}"
    );
}

#[test]
fn respond_reencodes_the_success_in_the_client_protocol() {
    let reply = exchange()
        .respond(
            &provider_facts(201),
            br#"{
                "id": "chatcmpl-1",
                "object": "chat.completion",
                "created": 1,
                "model": "chat-small-2026-01-01",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "hello"},
                    "finish_reason": "stop"
                }]
            }"#,
        )
        .expect("the answer encodes");
    assert_eq!(
        reply.status(),
        200,
        "the client protocol, not the provider's 201, answers a complete request"
    );
    let body = document(reply.body());
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["model"], "chat-small-2026-01-01");
    assert_eq!(body["choices"][0]["message"]["content"], "hello");
}

#[test]
fn respond_renders_an_upstream_failure_in_the_client_envelope() {
    let reply = exchange()
        .respond(
            &provider_facts(429),
            br#"{"error":{"message":"slow down, customer","type":"rate_limit","code":"rate_limit_exceeded"}}"#,
        )
        .expect("a failure status still produces a client answer");
    assert_eq!(reply.status(), 429);
    let body = document(reply.body());
    assert_eq!(body["error"]["code"], "upstream_rate_limited");
    assert!(
        !String::from_utf8_lossy(reply.body()).contains("slow down, customer"),
        "the provider's message must not be forwarded"
    );
}

#[test]
fn a_provider_failure_that_is_not_a_status_is_an_exchange_error() {
    let error = exchange()
        .respond(&provider_facts(200), b"not json")
        .expect_err("a 2xx body the adapter cannot read is not a client answer");
    assert!(matches!(
        error,
        ExchangeError::Provider(ProviderError::Malformed { .. })
    ));
    assert_eq!(error.canonical().http_status(), 502);
}

#[test]
fn stream_drives_the_provider_chunks_to_client_sse() {
    let mut stream = exchange().stream();
    let mut bytes = Vec::new();
    for chunk in [
        json!({
            "id": "chatcmpl-1",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": "chat-small-2026-01-01",
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": null}],
        }),
        json!({"choices": [{"index": 0, "delta": {"content": "Hi"}, "finish_reason": null}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
    ] {
        bytes.extend(
            stream
                .push(frame(&chunk.to_string()).as_bytes())
                .expect("the chunk converts"),
        );
    }
    bytes.extend(
        stream
            .push(frame("[DONE]").as_bytes())
            .expect("the terminator converts"),
    );
    bytes.extend(stream.finish().expect("the stream terminates"));

    let text = String::from_utf8(bytes).expect("client SSE is UTF-8");
    assert!(
        text.contains(r#""object":"chat.completion.chunk""#),
        "the client sees its own chunk shape: {text}"
    );
    assert!(text.contains(r#""content":"Hi""#), "{text}");
    assert!(text.contains(r#""finish_reason":"stop""#), "{text}");
    assert!(
        text.ends_with("data: [DONE]\n\n"),
        "the client stream always carries its terminator: {text}"
    );
}

#[test]
fn a_failed_provider_stream_ends_the_client_stream_with_an_error_frame() {
    let mut stream = exchange().stream();
    let start = frame(&json!({
        "id": "chatcmpl-1",
        "model": "chat-small-2026-01-01",
        "choices": [{"index": 0, "delta": {"role": "assistant", "content": "Hi"}, "finish_reason": null}],
    })
    .to_string());
    stream.push(start.as_bytes()).expect("the start converts");
    let failed = frame(
        &json!({
            "error": {"message": "secret detail", "type": "server_error", "code": "provider_x"},
        })
        .to_string(),
    );
    let bytes = stream
        .push(failed.as_bytes())
        .expect("the error frame converts");
    let text = String::from_utf8(bytes).expect("client SSE is UTF-8");
    assert!(text.contains(r#""code":"upstream_error""#), "{text}");
    assert!(
        !text.contains("secret detail"),
        "the provider's message must not be forwarded: {text}"
    );
    let tail = stream.finish().expect("a failed stream ends");
    assert!(
        !String::from_utf8_lossy(&tail).contains("[DONE]"),
        "a failed stream stops; it does not pretend to have finished"
    );
}

#[test]
fn an_image_the_provider_cannot_place_is_refused_as_an_unsupported_field() {
    // The OpenAI client codec accepts an image part in any role, so this document reaches the
    // adapter. The adapter cannot carry it in an assistant turn, and a well-formed document the
    // route cannot serve is a `422`, not a gateway defect.
    let body = br#"{
        "model": "chat-small",
        "messages": [
            {"role": "user", "content": "describe"},
            {
                "role": "assistant",
                "content": [
                    {"type": "image_url", "image_url": {"url": "https://example.test/MARKER.png"}}
                ]
            }
        ]
    }"#;
    let error = exchange()
        .prepare(&client_facts(), body)
        .expect_err("an assistant image has no field in this protocol");
    assert!(
        matches!(
            &error,
            ExchangeError::Provider(ProviderError::UnsupportedField { field, .. })
                if field == "message.content"
        ),
        "{error:?}"
    );
    let reply = exchange()
        .error_reply(&error)
        .expect("the client codec renders the refusal");
    assert_eq!(reply.status(), 422);
    let body = document(reply.body());
    assert_eq!(body["error"]["code"], "unsupported_capability");
    assert!(
        !String::from_utf8_lossy(reply.body()).contains("MARKER"),
        "a refusal must not quote the value it refused"
    );
}

#[test]
fn a_provider_stream_without_its_own_terminator_still_ends_the_client_stream() {
    // `[DONE]` is the provider's redundant sentinel; the client codec always writes its own at the
    // end of the body. A compatible endpoint that omits the sentinel must not truncate the answer.
    let mut stream = exchange().stream();
    let mut bytes = Vec::new();
    for chunk in [
        json!({
            "id": "chatcmpl-1",
            "model": "chat-small-2026-01-01",
            "choices": [{"index": 0, "delta": {"content": "Hi"}, "finish_reason": null}],
        }),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
    ] {
        bytes.extend(
            stream
                .push(frame(&chunk.to_string()).as_bytes())
                .expect("the chunk converts"),
        );
    }
    bytes.extend(stream.finish().expect("the stream terminates"));
    let text = String::from_utf8(bytes).expect("client SSE is UTF-8");
    assert!(text.contains(r#""finish_reason":"stop""#), "{text}");
    assert!(
        text.ends_with("data: [DONE]\n\n"),
        "the client always receives its terminator: {text}"
    );
}

#[test]
fn the_adapter_is_usable_through_the_provider_trait_object() {
    // The adapter is usable through the `Provider` trait object the publication stores.
    let route = route_with(None, &[]);
    let adapter: Arc<dyn nico_llm::Provider> = Arc::new(OpenAiProvider::new());
    let endpoint = adapter
        .endpoint(&route)
        .expect("a route without a credential still addresses an endpoint");
    assert_eq!(endpoint.destination().host(), "127.0.0.1");
    assert_eq!(provider().capabilities(), adapter.capabilities());
}
