//! The adapter inside the exchange: the pinned model, the stream, and the client's envelope.

use std::sync::Arc;

use nico_llm::{
    EndpointError, Exchange, ExchangeBuildError, ExchangeError, ExchangeMode, ModelId,
    PassthroughRoute, Provider, ProviderError, ResponseFacts, StreamEvent,
    provider::gemini::PROVIDER_KIND,
};
use serde_json::{Value, json};

use crate::support::{
    BASE_URL, LOGICAL, MODEL_VERSION, PHYSICAL, client_body, codec_arc, provider, provider_arc,
    request_facts, route, route_with, stream,
};

/// The request path this adapter pins for one model.
fn stream_path(physical: &str) -> String {
    format!("/v1beta/models/{physical}:streamGenerateContent?alt=sse")
}

/// Builds the exchange the data plane would hold for one route.
fn exchange() -> Exchange {
    Exchange::try_new(
        codec_arc(),
        provider_arc(),
        Arc::new(route_with(
            BASE_URL,
            Some("goog-secret"),
            &[(LOGICAL, PHYSICAL)],
        )),
    )
    .expect("the codec, the adapter and the route pair")
}

/// Parses a JSON body.
fn document(body: &[u8]) -> Value {
    serde_json::from_slice(body).expect("the body is JSON")
}

#[test]
fn a_request_that_resolves_to_another_model_is_refused_with_a_forbidden() {
    // The endpoint's path already names a model, so answering with the caller's bytes would serve a
    // different model than the one asked for. That is a route that cannot serve the request, not a
    // gateway defect: `403 model_not_allowed`.
    let exchange = exchange();
    let error = exchange
        .prepare(&request_facts(), &client_body("gemini-2.0-pro"))
        .expect_err("the pinned model is not the one the request resolved to");
    let model = ModelId::try_new("gemini-2.0-pro").expect("a valid name");
    assert_eq!(error, ExchangeError::ModelNotAllowed { model });
    let canonical = error.canonical();
    assert_eq!(canonical.http_status(), 403);
    assert_eq!(canonical.code(), "model_not_allowed");
}

#[test]
fn a_request_that_resolves_to_the_pinned_model_exchanges_normally() {
    let exchange = exchange();
    let prepared = exchange
        .prepare(&request_facts(), &client_body(LOGICAL))
        .expect("the logical name resolves to the pinned model");
    assert_eq!(prepared.model().logical().as_str(), LOGICAL);
    assert_eq!(prepared.model().physical().as_str(), PHYSICAL);
    assert_eq!(
        prepared.wire().endpoint().path(),
        stream_path(PHYSICAL),
        "the upstream target is the provider's, not the client's"
    );
    assert_eq!(
        prepared.wire().endpoint().model().map(ModelId::as_str),
        Some(PHYSICAL)
    );
    let body = document(prepared.wire().body());
    assert_eq!(body.get("model"), None, "the path carries the model");
    assert_eq!(
        body["contents"][0]["parts"][0]["text"], "PROMPT-MARKER",
        "the client's message reached the provider's document"
    );
}

#[test]
fn the_endpoint_is_available_before_the_body_is_decoded() {
    let exchange = exchange();
    let endpoint = exchange.endpoint();
    assert_eq!(endpoint.path(), stream_path(PHYSICAL));
    assert_eq!(
        endpoint
            .headers()
            .iter()
            .find(|(name, _)| name == "x-goog-api-key")
            .map(|(_, value)| value.as_str()),
        Some("goog-secret")
    );
}

#[test]
fn a_prepared_request_debug_names_nothing_from_the_prompt_or_the_credential() {
    let prepared = exchange()
        .prepare(&request_facts(), &client_body(LOGICAL))
        .expect("the request exchanges");
    let record = format!("{prepared:?}");
    assert!(
        !record.contains("PROMPT-MARKER"),
        "the conversation is the caller's prompt: {record}"
    );
    assert!(
        !record.contains("goog-secret"),
        "the credential must not be printed: {record}"
    );
}

#[test]
fn a_route_without_a_constant_model_is_refused_when_the_pair_is_built() {
    // The endpoint depends on route facts only, so an unservable route is a publication failure that
    // keeps the last known good document serving — not a 500 per request.
    let error = Exchange::try_new(
        codec_arc(),
        provider_arc(),
        Arc::new(route_with(BASE_URL, None, &[])),
    )
    .expect_err("the adapter cannot address this route");
    assert!(
        matches!(
            error,
            ExchangeBuildError::Endpoint {
                source: ProviderError::InvalidEndpoint {
                    source: EndpointError::RouteFact { .. },
                    ..
                },
            }
        ),
        "{error:?}"
    );
}

#[test]
fn a_passthrough_route_is_refused_at_publication_because_its_target_cannot_follow_the_caller() {
    // The passthrough front-end is what a `mode: passthrough` route compiles into, and this adapter
    // refuses it: the caller's target carries the model and the operation, while a passthrough route
    // forwards bytes to a target fixed at publication.
    let route = route(BASE_URL).with_mode(ExchangeMode::Passthrough);
    let error = PassthroughRoute::try_new(Arc::new(route), &provider())
        .expect_err("the adapter refuses passthrough");
    assert!(
        matches!(
            error,
            ExchangeBuildError::Endpoint {
                source: ProviderError::InvalidEndpoint {
                    source: EndpointError::RouteFact {
                        field: "routes[].mode",
                        ..
                    },
                    ..
                },
            }
        ),
        "{error:?}"
    );
}

#[test]
fn respond_folds_the_providers_event_stream_and_leaves_the_status_to_the_codec() {
    let body = stream(&[json!({
        "candidates": [{"content": {"parts": [{"text": "hello"}]}, "finishReason": "STOP"}],
        "modelVersion": MODEL_VERSION,
        "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1}
    })]);
    let reply = exchange()
        .respond(&ResponseFacts::new(200, Vec::new()), body.as_bytes())
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
            br#"{"error":{"code":429,"message":"quota for customer 9","status":"RESOURCE_EXHAUSTED"}}"#,
        )
        .expect("a failure status still produces a client answer");
    assert_eq!(reply.status(), 429);
    assert_eq!(document(reply.body())["code"], "upstream_rate_limited");
    assert!(
        !String::from_utf8_lossy(reply.body()).contains("customer 9"),
        "the provider's message must not be forwarded"
    );
}

#[test]
fn a_provider_body_that_is_not_an_answer_is_an_exchange_error() {
    let error = exchange()
        .respond(&ResponseFacts::new(200, Vec::new()), b"not an event stream")
        .expect_err("a 2xx body the adapter cannot read is not a client answer");
    assert!(matches!(
        error,
        ExchangeError::Provider(ProviderError::Malformed { .. })
    ));
    assert_eq!(error.canonical().http_status(), 502);
}

#[test]
fn the_streamed_and_buffered_paths_report_the_same_answer() {
    // The two paths share the decoder: the stream returns the events, the buffered path folds them.
    let chunks = [
        json!({
            "candidates": [{"content": {"parts": [{"text": "one "}]}}],
            "modelVersion": MODEL_VERSION,
            "responseId": "resp-7"
        }),
        json!({
            "candidates": [{"content": {"parts": [{"text": "two"}]}, "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 2, "candidatesTokenCount": 2}
        }),
    ];
    let body = stream(&chunks);
    let buffered = provider()
        .decode_response(&ResponseFacts::new(200, Vec::new()), body.as_bytes())
        .expect("the body folds");

    let mut decoder = provider().stream_decoder();
    let mut events = decoder
        .push(body.as_bytes())
        .expect("the stream is readable");
    events.extend(decoder.finish().expect("the stream terminates"));

    let text: String = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::TextDelta(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "one two");
    assert_eq!(buffered.id().as_str(), "resp-7");
    assert_eq!(
        buffered.choices()[0].finish_reason(),
        nico_llm::FinishReason::Stop
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, StreamEvent::Finish(nico_llm::FinishReason::Stop))),
        "the stream's finish event is what the fold reads"
    );
    assert_eq!(
        buffered
            .usage()
            .reported_counts()
            .and_then(|counts| counts.get(nico_llm::TokenKind::Output)),
        Some(2)
    );
}

#[test]
fn the_exchange_answers_a_status_without_a_body() {
    // The transform needs the failure before any body byte exists, so the exchange exposes the same
    // table the response path uses.
    let error = exchange().error_for_status(503);
    assert_eq!(error.code(), "upstream_error");
    assert_eq!(error.http_status(), 502);
    assert!(error.retryable());
}

#[test]
fn the_adapter_pairs_with_a_codec_it_does_not_know() {
    // The two axes are independent: a route pairs a codec with an adapter by protocol identifier, and
    // the adapter never learns what the codec is. The Gemini *client* codec does not exist in this
    // build, and the exchange does not need it.
    let exchange = exchange();
    assert_eq!(exchange.route().provider_kind(), PROVIDER_KIND);
    assert_eq!(exchange.route().protocol(), provider().native_protocol());
}
