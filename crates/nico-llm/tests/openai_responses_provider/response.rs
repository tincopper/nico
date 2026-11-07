//! Decoding a Responses body into canonical values: output items, status, usage and refusals.

use nico_llm::{
    FinishReason, Provider, TokenKind, Usage, provider::openai_responses::PROVIDER_KIND,
};
use serde_json::{Value, json};

use crate::support::{decode_err, decoded, facts, provider};

/// Decodes one document that must be accepted.
fn response() -> nico_llm::ChatResponse {
    decoded(&document())
}

/// A complete answer with one text message.
fn document() -> String {
    json!({
        "id": "resp_1",
        "object": "response",
        "created_at": 1741476542,
        "status": "completed",
        "model": "gpt-small-2026-01-01",
        "output": [{
            "type": "message",
            "id": "msg_1",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "hello", "annotations": []}],
        }],
        "usage": {
            "input_tokens": 10,
            "output_tokens": 5,
            "total_tokens": 15,
            "input_tokens_details": {"cached_tokens": 3},
            "output_tokens_details": {"reasoning_tokens": 2},
        },
    })
    .to_string()
}

/// Replaces one top-level member of the base document.
fn with(member: &str, value: Value) -> String {
    let mut document: Value = serde_json::from_str(&document()).expect("the fixture is JSON");
    document[member] = value;
    document.to_string()
}

/// Adds one `output[]` item to the base document.
fn with_item(item: Value) -> String {
    let mut document: Value = serde_json::from_str(&document()).expect("the fixture is JSON");
    document["output"] = json!([item]);
    document.to_string()
}

mod items;
#[test]
fn a_complete_answer_is_one_assistant_turn() {
    let response = response();
    assert_eq!(response.id().as_str(), "resp_1");
    assert_eq!(response.model().as_str(), "gpt-small-2026-01-01");
    assert_eq!(response.choices().len(), 1);
    let choice = &response.choices()[0];
    assert_eq!(choice.index(), 0);
    assert_eq!(choice.finish_reason(), FinishReason::Stop);
    assert_eq!(choice.turn().content().len(), 1);
    assert!(choice.turn().tool_calls().is_empty());
    assert!(choice.turn().reasoning().is_none());
}

#[test]
fn the_reported_counts_are_read_into_the_canonical_usage() {
    let response = response();
    assert_eq!(
        response.usage().reported_counts(),
        Some(
            &nico_llm::UsageCounts::new()
                .with(TokenKind::Input, 10)
                .with(TokenKind::Output, 5)
                .with(TokenKind::CacheRead, 3)
                .with(TokenKind::Reasoning, 2)
        ),
        "`total_tokens` is a sum the canonical model does not carry twice"
    );
    assert_eq!(response.usage().measured_counts(), None);
}

#[test]
fn a_usage_document_with_no_count_is_not_a_reported_zero() {
    let body = with("usage", json!({}));
    assert_eq!(
        decoded(&body).usage(),
        &Usage::default(),
        "\"the provider said nothing\" and \"the provider said zero\" are different facts"
    );
}

#[test]
fn incomplete_output_reports_why_generation_stopped() {
    for (reason, expected) in [
        ("max_output_tokens", FinishReason::Length),
        ("content_filter", FinishReason::ContentFilter),
    ] {
        let body = with("status", json!("incomplete"));
        let mut document: Value = serde_json::from_str(&body).expect("the fixture is JSON");
        document["incomplete_details"] = json!({"reason": reason});
        let response = decoded(&document.to_string());
        assert_eq!(
            response.choices()[0].finish_reason(),
            expected,
            "`{reason}` must map to {expected:?}"
        );
    }
}

#[test]
fn an_incomplete_answer_without_a_reason_is_refused() {
    let body = with("status", json!("incomplete"));
    assert!(
        matches!(
            decode_err(200, &body),
            nico_llm::ProviderError::Malformed { .. }
        ),
        "the caller must not be told why generation stopped when nobody said"
    );
}

#[test]
fn an_unknown_incomplete_reason_is_refused_rather_than_mapped_to_stop() {
    let mut document: Value =
        serde_json::from_str(&with("status", json!("incomplete"))).expect("the fixture is JSON");
    document["incomplete_details"] = json!({"reason": "something_new"});
    assert!(matches!(
        decode_err(200, &document.to_string()),
        nico_llm::ProviderError::Malformed { .. }
    ));
}

#[test]
fn a_failed_answer_is_an_answer_shaped_failure() {
    // The provider served the request and reported that generation failed; the caller reads that in
    // its own protocol rather than as a gateway error.
    let body = with("status", json!("failed"));
    let mut document: Value = serde_json::from_str(&body).expect("the fixture is JSON");
    document["error"] = json!({"code": "server_error", "message": "PROVIDER-MARKER"});
    let response = decoded(&document.to_string());
    assert_eq!(response.choices()[0].finish_reason(), FinishReason::Error);
    assert!(
        !format!("{response:?}").contains("PROVIDER-MARKER"),
        "the provider's message never reaches the canonical answer"
    );
}

#[test]
fn a_document_that_is_not_a_finished_answer_is_refused() {
    for status in ["in_progress", "queued", "cancelled", "something_new"] {
        let body = with("status", json!(status));
        assert!(
            matches!(
                decode_err(200, &body),
                nico_llm::ProviderError::Malformed { .. }
            ),
            "`{status}` must be refused"
        );
    }
}

#[test]
fn the_answer_must_name_itself() {
    for member in ["id", "model", "status", "output"] {
        let mut document: Value = serde_json::from_str(&document()).expect("the fixture is JSON");
        document.as_object_mut().expect("an object").remove(member);
        assert!(
            matches!(
                decode_err(200, &document.to_string()),
                nico_llm::ProviderError::Malformed { .. }
            ),
            "a document with no `{member}` is not a complete answer"
        );
    }
}

#[test]
fn a_document_of_another_object_type_is_refused() {
    let body = with("object", json!("chat.completion"));
    assert!(matches!(
        decode_err(200, &body),
        nico_llm::ProviderError::Malformed { .. }
    ));
}

#[test]
fn an_unknown_top_level_member_is_preserved() {
    let body = with("future_field", json!({"a": 1}));
    assert_eq!(
        decoded(&body).extensions().get("future_field").cloned(),
        Some(nico_llm::RawJson::new(json!({"a": 1})))
    );
}

#[test]
fn the_request_echo_is_consumed_instead_of_preserved() {
    // The dialect repeats request members beside the answer. Writing one into a client's document
    // would report a fact the caller never asked to have relayed.
    let body = with(
        "tools",
        json!([{"type": "function", "name": "lookup", "parameters": {"type": "object"}}]),
    );
    let mut document: Value = serde_json::from_str(&body).expect("the fixture is JSON");
    document["instructions"] = json!("be brief");
    document["metadata"] = json!({"tenant": "t1"});
    document["text"] = json!({"format": {"type": "text"}});
    let response = decoded(&document.to_string());
    for member in ["tools", "instructions", "metadata", "text"] {
        assert!(
            response.extensions().get(member).is_none(),
            "`{member}` echoes the request and must not be preserved"
        );
    }
}

#[test]
fn the_envelope_members_are_not_preserved_either() {
    let response = response();
    for member in ["object", "created_at", "error"] {
        assert!(
            response.extensions().get(member).is_none(),
            "`{member}` is framing, not content"
        );
    }
}

#[test]
fn a_failure_status_becomes_a_canonical_failure_without_the_provider_body() {
    let error = provider()
        .decode_response(
            &facts(429),
            br#"{"error":{"code":"rate_limit_exceeded","message":"PROVIDER-MARKER"}}"#,
        )
        .expect_err("a failure status is not an answer");
    match &error {
        nico_llm::ProviderError::Upstream { error } => {
            assert_eq!(error.http_status(), 429);
            assert_eq!(error.code(), "upstream_rate_limited");
            assert_eq!(error.provider_code(), Some("rate_limit_exceeded"));
        }
        other => panic!("a failure status must be an upstream failure: {other:?}"),
    }
    assert!(
        !format!("{error:?} {error}").contains("PROVIDER-MARKER"),
        "the provider's message never reaches the caller: {error}"
    );
}

#[test]
fn the_adapter_reports_the_same_status_without_a_body() {
    // The response head is written before the body is read, so both paths must agree.
    let from_body = provider()
        .decode_response(&facts(503), b"{}")
        .expect_err("a failure status is not an answer");
    let nico_llm::ProviderError::Upstream { error } = from_body else {
        panic!("a failure status must be an upstream failure");
    };
    assert_eq!(provider().error_for_status(503), error);
    assert_eq!(PROVIDER_KIND.as_str(), "openai_responses");
}
