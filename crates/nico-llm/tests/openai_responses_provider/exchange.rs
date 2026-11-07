//! The adapter inside the exchange: the codec on the other side is not this adapter's business.

use std::sync::Arc;

use nico_llm::{
    Exchange, Provider, ProviderError, ProviderRoute, ResponseFacts,
    provider::openai_responses::{OpenAiResponsesProvider, PROVIDER_KIND},
};
use serde_json::{Value, json};

use crate::support::{
    BASE_URL, codec, codec_arc, provider, provider_arc, request_facts, route, route_with,
};

/// Builds the exchange the data plane would hold for one route.
fn exchange() -> Exchange {
    exchange_on(Some("sk-responses-secret"))
}

/// Builds the exchange for a route with an optional credential.
fn exchange_on(credential: Option<&str>) -> Exchange {
    Exchange::try_new(
        codec_arc(),
        provider_arc(),
        Arc::new(route_with(BASE_URL, credential)),
    )
    .expect("the codec, the adapter and the route pair")
}

/// The client request body the stub codec decodes.
fn client_body() -> Vec<u8> {
    br#"{"model":"gpt-small","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"PROMPT-MARKER"}]}]}"#.to_vec()
}

/// Parses a JSON body.
fn document(body: &[u8]) -> Value {
    serde_json::from_slice(body).expect("the body is JSON")
}

#[test]
fn a_route_needs_no_output_ceiling_to_be_published() {
    // `max_output_tokens` is optional in this dialect, so — unlike the Messages adapter — a route
    // without the route fact is addressable and nothing is refused at publication.
    let exchange = exchange_on(None);
    assert_eq!(exchange.endpoint().path(), "/v1/responses");
}

#[test]
fn the_endpoint_is_available_before_the_body_is_decoded() {
    let exchange = exchange();
    let endpoint = exchange.endpoint();
    assert_eq!(endpoint.path(), "/v1/responses");
    assert_eq!(
        endpoint
            .headers()
            .iter()
            .find(|(name, _)| name == "authorization")
            .map(|(_, value)| value.as_str()),
        Some("Bearer sk-responses-secret")
    );
}

#[test]
fn prepare_writes_the_physical_model_the_input_and_store_false() {
    let exchange = exchange();
    let prepared = exchange
        .prepare(&request_facts(), &client_body())
        .expect("the request exchanges");
    assert_eq!(prepared.model().logical().as_str(), "gpt-small");
    assert_eq!(
        prepared.wire().endpoint().path(),
        "/v1/responses",
        "the endpoint is the provider's, not the client's"
    );
    let body = document(prepared.wire().body());
    assert_eq!(body["model"], "gpt-small");
    assert_eq!(
        body["input"][0],
        json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "PROMPT-MARKER"}],
        })
    );
    assert_eq!(
        body["store"], false,
        "the gateway never asks the upstream to retain an answer"
    );
}

#[test]
fn a_prepared_request_debug_names_nothing_from_the_prompt_or_the_credential() {
    let prepared = exchange()
        .prepare(&request_facts(), &client_body())
        .expect("the request exchanges");
    let record = format!("{prepared:?}");
    assert!(
        !record.contains("PROMPT-MARKER"),
        "the conversation is the caller's prompt: {record}"
    );
    assert!(
        !record.contains("sk-responses-secret"),
        "the credential must not be printed: {record}"
    );
}

#[test]
fn respond_decodes_the_answer_and_leaves_the_status_to_the_codec() {
    let reply = exchange()
        .respond(
            &ResponseFacts::new(200, Vec::new()),
            br#"{
                "id": "resp_1",
                "object": "response",
                "status": "completed",
                "model": "gpt-small-2026-01-01",
                "output": [{
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "hello"}]
                }],
                "usage": {"input_tokens": 3, "output_tokens": 2}
            }"#,
        )
        .expect("the answer encodes");
    assert_eq!(
        reply.status(),
        200,
        "the client protocol, not the provider's status, answers a complete request"
    );
}

#[test]
fn respond_renders_an_upstream_failure_in_the_client_envelope() {
    let reply = exchange()
        .respond(
            &ResponseFacts::new(429, Vec::new()),
            br#"{"error":{"code":"rate_limit_exceeded","message":"slow down, customer"}}"#,
        )
        .expect("a failure status still produces a client answer");
    assert_eq!(reply.status(), 429);
    let body = document(reply.body());
    assert_eq!(body["code"], "upstream_rate_limited");
    assert!(
        !String::from_utf8_lossy(reply.body()).contains("slow down, customer"),
        "the provider's message must not be forwarded"
    );
}

#[test]
fn a_provider_failure_that_is_not_a_status_is_an_exchange_error() {
    let error = exchange()
        .respond(&ResponseFacts::new(200, Vec::new()), b"not json")
        .expect_err("a 2xx body the adapter cannot read is not a client answer");
    assert!(matches!(
        error,
        nico_llm::ExchangeError::Provider(ProviderError::Malformed { .. })
    ));
    assert_eq!(error.canonical().http_status(), 502);
}

#[test]
fn the_exchange_answers_a_status_without_a_body() {
    // The transform needs the failure before any body byte exists, so the exchange exposes the same
    // table the response path uses.
    let error = exchange().error_for_status(529);
    assert_eq!(error.code(), "upstream_error");
    assert_eq!(error.http_status(), 502);
    assert!(error.retryable());
}

#[test]
fn the_adapter_pairs_with_a_codec_it_does_not_know() {
    // The two axes are independent: a route pairs a codec with an adapter by protocol identifier,
    // and the adapter never learns what the codec is. The Responses *client* codec does not exist in
    // this build, and the exchange does not need it.
    let stub = codec();
    assert_eq!(
        nico_llm::ClientCodec::protocol(&stub),
        provider().native_protocol(),
        "the route's protocol is both sides' agreement"
    );
    let exchange = exchange();
    assert_eq!(exchange.route().provider_kind(), PROVIDER_KIND);
    assert_eq!(exchange.route().protocol(), provider().native_protocol());
    let _: &dyn Provider = &OpenAiResponsesProvider::new();
    let _: ProviderRoute = route(BASE_URL);
}
