//! One converting route: the facts it publishes and the failure a caller sees.

use std::sync::Arc;

use nico_core::ErrorCategory;
use nico_http::{
    error::HttpError,
    llm::{ConvertingRoute, LlmRoute},
};
use nico_llm::route::ConvertRoute;
use serde_json::Value;

use crate::support::{
    BASE_URL, FailingCodec, chain_of, client_body, convert_route, exchange, exchange_with,
    provider, view,
};

/// Parses a document a fixture produced.
fn document(body: &[u8]) -> Value {
    serde_json::from_slice(body).expect("the fixture body is JSON")
}

/// Unwraps a refusal, failing the test with the unexpected answer.
fn refused(error: HttpError) -> Arc<nico_http::error::LlmRejection> {
    match error {
        HttpError::LlmRejected { rejection } => rejection,
        other => panic!("expected the client protocol's error document, got {other:?}"),
    }
}

#[test]
fn the_route_reports_the_facts_the_upstream_head_needs() {
    let route = convert_route();

    assert!(
        route.converting().is_some(),
        "a converting route reports its exchange"
    );
    assert_eq!(route.mode_name(), "convert");
    assert_eq!(route.protocol(), "openai.chat");
    assert_eq!(route.provider(), "openai");
    assert_eq!(route.destination().host(), "127.0.0.1");
    assert_eq!(route.destination().port(), 8080);
    assert_eq!(
        route.destination().scheme(),
        nico_core::Scheme::Http,
        "the endpoint is the adapter's parse of {BASE_URL}"
    );
    assert_eq!(route.path(), "/v1/chat/completions");
    assert_eq!(
        route
            .headers()
            .iter()
            .find(|(name, _)| name == "authorization")
            .map(|(_, value)| value.as_str()),
        Some("Bearer sk-test-secret")
    );
}

#[test]
fn prepare_decodes_the_body_and_reports_both_model_names() {
    let prepared = convert_route()
        .prepare(&view(&client_body(false)))
        .expect("the request exchanges");

    assert!(!prepared.streaming());
    assert_eq!(
        prepared.model(),
        Some("chat-small -> chat-small-2026-01-01"),
        "the request record names the logical model and the one the provider receives"
    );
    let body = document(prepared.body());
    assert_eq!(body["model"], "chat-small-2026-01-01");
    assert_eq!(body["messages"][0]["content"], "PROMPT-MARKER");
}

#[test]
fn prepare_reports_the_stream_flag_the_client_asked_for() {
    let prepared = convert_route()
        .prepare(&view(&client_body(true)))
        .expect("the request exchanges");

    assert!(
        prepared.streaming(),
        "the data plane decides how to translate the response from this flag"
    );
    assert_eq!(document(prepared.body())["stream"], true);
}

#[test]
fn a_request_the_route_cannot_serve_keeps_the_client_error_document() {
    // The OpenAI codec accepts an image in an assistant turn; the adapter cannot express it there.
    // The document is valid, the route cannot serve it, and the caller must receive its own 422
    // envelope rather than this data plane's generic one.
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
    let rejection = refused(
        convert_route()
            .prepare(&view(body))
            .expect_err("the adapter cannot place an assistant image"),
    );

    assert_eq!(rejection.status(), 422);
    assert_eq!(rejection.code(), "unsupported_capability");
    assert_eq!(rejection.category(), ErrorCategory::Client);
    assert_eq!(
        document(rejection.body())["error"]["code"],
        "unsupported_capability"
    );
    assert!(
        !String::from_utf8_lossy(rejection.body()).contains("MARKER"),
        "the document is the codec's, and it does not quote the value it refused"
    );
}

#[test]
fn a_malformed_client_document_is_answered_400_in_the_client_envelope() {
    let rejection = refused(
        convert_route()
            .prepare(&view(b"not a document"))
            .expect_err("the codec cannot read this body"),
    );

    assert_eq!(rejection.status(), 400);
    assert_eq!(rejection.code(), "invalid_request");
    assert_eq!(
        document(rejection.body())["error"]["type"],
        "invalid_request_error",
        "the status and the envelope are the client protocol's"
    );
}

#[test]
fn a_codec_that_cannot_encode_its_error_is_a_gateway_defect() {
    // The exchange maps the refusal to a canonical 400, but the codec cannot render it. Re-encoding
    // it here as the generic envelope would report a status nobody chose; the honest answer is 500.
    let exchange = exchange_with(Arc::new(FailingCodec), Arc::new(provider()));
    let route = ConvertRoute::new(chain_of(exchange), crate::support::breakers(), 1);
    let error = route
        .prepare(&view(&client_body(false)))
        .expect_err("the fixture codec refuses every document");

    assert!(
        matches!(error, HttpError::Internal { .. }),
        "a codec that cannot render its own failure is this side's defect: {error:?}"
    );
}

#[test]
fn the_route_debug_names_nothing_from_the_prompt_or_the_credential() {
    let route: ConvertRoute =
        ConvertRoute::new(chain_of(exchange()), crate::support::breakers(), 1);
    let printed = format!("{route:?}");

    assert!(printed.contains("ConvertRoute"), "{printed}");
    assert!(printed.contains("openai.chat"), "{printed}");
    assert!(
        !printed.contains("sk-test-secret"),
        "the credential must not be printed: {printed}"
    );
    assert!(
        !printed.contains("PROMPT-MARKER"),
        "the route does not even hold the prompt: {printed}"
    );
}

#[test]
fn the_route_is_usable_through_the_trait_object_the_state_publishes() {
    // The published value is an `Arc<dyn LlmRoute>`; the mode and the two protocol names are what the
    // data plane reports for a request.
    let route: Arc<dyn LlmRoute> = Arc::new(convert_route());
    assert!(
        route.converting().is_some(),
        "a converting route reports its exchange"
    );
    assert_eq!(route.mode_name(), "convert");
    assert_eq!(route.protocol(), "openai.chat");
    assert_eq!(route.provider(), "openai");
}
