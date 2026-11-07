//! The buffered path: a whole `streamGenerateContent` body folded into one response.

use nico_core::ErrorCategory;
use nico_llm::{
    ContentPart, FinishReason, ProviderError, RawJson, StreamEvent, TokenKind,
    provider::gemini::DEFAULT_RESPONSE_ID,
};
use serde_json::{Value, json};

use crate::support::{MODEL_VERSION, RESPONSE_ID, decode_err, decoded, events_of, stream};

/// Decodes one answer built from the given chunks.
fn answer(chunks: &[Value]) -> nico_llm::ChatResponse {
    decoded(&stream(chunks))
}

/// Returns the text the answer's turn carries.
fn text(response: &nico_llm::ChatResponse) -> String {
    response.choices()[0]
        .turn()
        .content()
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_complete_answer_is_folded_from_its_chunks() {
    let response = answer(&[
        json!({
            "candidates": [{"content": {"role": "model", "parts": [{"text": "Hel"}]}}],
            "modelVersion": MODEL_VERSION,
            "responseId": RESPONSE_ID
        }),
        json!({
            "candidates": [{"content": {"parts": [{"text": "lo"}]}, "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 2}
        }),
    ]);
    assert_eq!(response.id().as_str(), RESPONSE_ID);
    assert_eq!(response.model().as_str(), MODEL_VERSION);
    assert_eq!(text(&response), "Hello");
    assert_eq!(
        response.choices()[0].finish_reason(),
        FinishReason::Stop,
        "this dialect reports `STOP` after a function call too, so the reason alone is not a call"
    );
    let counts = response.usage().reported_counts().expect("usage reported");
    assert_eq!(counts.get(TokenKind::Input), Some(3));
    assert_eq!(counts.get(TokenKind::Output), Some(2));
}

#[test]
fn an_answer_without_a_response_id_is_reported_under_the_stable_label() {
    let response = answer(&[json!({
        "candidates": [{"content": {"parts": [{"text": "hi"}]}, "finishReason": "STOP"}],
        "modelVersion": MODEL_VERSION
    })]);
    assert_eq!(response.id().as_str(), DEFAULT_RESPONSE_ID);
}

#[test]
fn an_answer_that_names_no_model_is_refused() {
    // The canonical answer names the model that produced it, and this dialect reports it per chunk.
    let error = decode_err(
        200,
        &stream(&[json!({
            "candidates": [{"content": {"parts": [{"text": "hi"}]}, "finishReason": "STOP"}]
        })]),
    );
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
}

#[test]
fn every_finish_reason_has_its_canonical_value() {
    for (wire, expected) in [
        ("STOP", FinishReason::Stop),
        ("MAX_TOKENS", FinishReason::Length),
        ("SAFETY", FinishReason::ContentFilter),
        ("RECITATION", FinishReason::ContentFilter),
        ("BLOCKLIST", FinishReason::ContentFilter),
        ("PROHIBITED_CONTENT", FinishReason::ContentFilter),
        ("SPII", FinishReason::ContentFilter),
        ("MALFORMED_FUNCTION_CALL", FinishReason::Error),
        ("OTHER", FinishReason::Error),
    ] {
        let response = answer(&[json!({
            "candidates": [{"finishReason": wire}],
            "modelVersion": MODEL_VERSION
        })]);
        assert_eq!(response.choices()[0].finish_reason(), expected, "`{wire}`");
    }
}

#[test]
fn a_finish_reason_this_adapter_does_not_know_is_refused() {
    let error = decode_err(
        200,
        &stream(&[json!({
            "candidates": [{"finishReason": "TOOL_CALL"}],
            "modelVersion": MODEL_VERSION
        })]),
    );
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "mapping an unknown reason onto the nearest one would say something the provider did not: \
         {error:?}"
    );
}

#[test]
fn more_than_one_candidate_is_refused_rather_than_merged() {
    let error = decode_err(
        200,
        &stream(&[json!({
            "candidates": [
                {"content": {"parts": [{"text": "a"}]}},
                {"content": {"parts": [{"text": "b"}]}}
            ],
            "modelVersion": MODEL_VERSION
        })]),
    );
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
}

#[test]
fn a_candidate_that_is_not_the_first_is_refused() {
    // The canonical stream addresses one answer per response, so a nonzero index cannot be placed.
    let error = decode_err(
        200,
        &stream(&[json!({
            "candidates": [{"index": 1, "content": {"parts": [{"text": "a"}]}}],
            "modelVersion": MODEL_VERSION
        })]),
    );
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
}

#[test]
fn content_that_is_not_a_model_turn_is_refused() {
    let error = decode_err(
        200,
        &stream(&[json!({
            "candidates": [{"content": {"role": "user", "parts": [{"text": "a"}]}}],
            "modelVersion": MODEL_VERSION
        })]),
    );
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
}

#[test]
fn every_reported_count_has_a_bucket() {
    let response = answer(&[json!({
        "candidates": [{"finishReason": "STOP"}],
        "usageMetadata": {
            "promptTokenCount": 10,
            "candidatesTokenCount": 4,
            "cachedContentTokenCount": 6,
            "thoughtsTokenCount": 2,
            "totalTokenCount": 16
        },
        "modelVersion": MODEL_VERSION
    })]);
    let counts = response.usage().reported_counts().expect("usage reported");
    assert_eq!(counts.get(TokenKind::Input), Some(10));
    assert_eq!(counts.get(TokenKind::Output), Some(4));
    assert_eq!(counts.get(TokenKind::CacheRead), Some(6));
    assert_eq!(counts.get(TokenKind::Reasoning), Some(2));
}

#[test]
fn a_blocked_prompt_is_a_content_filter_with_no_content() {
    // The provider refused the prompt before generating: there is no candidate and no finish reason,
    // and reporting a missing reason would call a provider decision an unreadable answer.
    let response = answer(&[json!({
        "promptFeedback": {"blockReason": "SAFETY"},
        "modelVersion": MODEL_VERSION
    })]);
    assert_eq!(
        response.choices()[0].finish_reason(),
        FinishReason::ContentFilter
    );
    assert!(response.choices()[0].turn().content().is_empty());
}

#[test]
fn a_folded_answer_carries_its_tool_call_as_arguments() {
    let response = answer(&[
        json!({
            "candidates": [{"content": {"role": "model", "parts": [
                {"functionCall": {"id": "call-1", "name": "weather", "args": {"city": "Paris"}}}
            ]}}],
            "modelVersion": MODEL_VERSION
        }),
        json!({"candidates": [{"finishReason": "STOP"}]}),
    ]);
    let calls = response.choices()[0].turn().tool_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id().as_str(), "call-1");
    assert_eq!(calls[0].name().as_str(), "weather");
    assert_eq!(
        calls[0].arguments(),
        &RawJson::new(json!({"city": "Paris"}))
    );
}

#[test]
fn a_folded_answer_renders_its_thought_as_reasoning_without_a_signature() {
    let response = answer(&[
        json!({
            "candidates": [{"content": {"role": "model", "parts": [
                {"text": "step one", "thought": true, "thoughtSignature": "sig-1"},
                {"text": "answer"}
            ]}}],
            "modelVersion": MODEL_VERSION
        }),
        json!({"candidates": [{"finishReason": "STOP"}]}),
    ]);
    let reasoning = response.choices()[0]
        .turn()
        .reasoning()
        .expect("the thought is part of the answer");
    assert_eq!(reasoning.text(), "step one");
    assert_eq!(
        reasoning.signature(),
        None,
        "the canonical event set has no place for the signature, so the fold cannot carry it"
    );
}

#[test]
fn an_unknown_envelope_member_is_consumed_rather_than_refused() {
    // This dialect adds metadata beside the answer; only a part is closed, because only a part is
    // content.
    let response = answer(&[
        json!({
            "candidates": [{
                "content": {"parts": [{"text": "hi"}]},
                "citationMetadata": {"citationSources": []},
                "safetyRatings": [{"category": "HARM_CATEGORY_DANGER"}]
            }],
            "modelVersion": MODEL_VERSION,
            "createTime": "2026-01-01T00:00:00Z"
        }),
        json!({"candidates": [{"finishReason": "STOP"}]}),
    ]);
    assert_eq!(text(&response), "hi");
}

#[test]
fn a_failure_status_is_mapped_without_reading_its_message() {
    let error = decode_err(
        429,
        r#"{"error":{"code":429,"message":"quota for customer project-9","status":"RESOURCE_EXHAUSTED"}}"#,
    );
    let ProviderError::Upstream { error } = error else {
        panic!("a failure status is the provider's failure: {error:?}");
    };
    assert_eq!(error.category(), ErrorCategory::Limit);
    assert_eq!(error.code(), "upstream_rate_limited");
    assert_eq!(error.http_status(), 429);
    assert!(error.retryable());
    assert_eq!(
        error.provider_code(),
        Some("RESOURCE_EXHAUSTED"),
        "the provider's own status token reaches the log, not the caller's message"
    );
    assert!(
        !format!("{error:?} {error}").contains("project-9"),
        "the provider's message must not be forwarded"
    );
}

#[test]
fn the_status_table_is_closed() {
    for (status, code, caller, retryable) in [
        (400, "upstream_rejected", 400, false),
        (401, "upstream_auth_failed", 502, false),
        (403, "upstream_auth_failed", 502, false),
        (404, "upstream_model_not_found", 404, false),
        (408, "upstream_timeout", 504, true),
        (418, "upstream_rejected", 418, false),
        (500, "upstream_error", 502, true),
        (503, "upstream_error", 502, true),
        (302, "upstream_error", 502, false),
    ] {
        let error = nico_llm::Provider::error_for_status(&crate::support::provider(), status);
        assert_eq!(error.code(), code, "status {status}");
        assert_eq!(error.http_status(), caller, "status {status}");
        assert_eq!(error.retryable(), retryable, "status {status}");
        assert_eq!(
            error.provider_code(),
            None,
            "the status table reads no body, so it can carry no provider token"
        );
    }
}

#[test]
fn a_body_that_is_not_an_event_stream_is_refused() {
    let error = decode_err(200, "not an event stream");
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
}

#[test]
fn the_streamed_and_buffered_paths_read_the_same_chunks() {
    // The buffered answer is the fold of the very events the stream path emits, so the two cannot
    // disagree about the answer.
    let chunks = [
        json!({
            "candidates": [{"content": {"role": "model", "parts": [{"text": "one "}]}}],
            "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1},
            "modelVersion": MODEL_VERSION
        }),
        json!({
            "candidates": [{"content": {"parts": [{"text": "two"}]}, "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 2}
        }),
    ];
    let response = answer(&chunks);
    let events = events_of(&chunks);
    let folded: String = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::TextDelta(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text(&response), folded);
    assert_eq!(
        response.choices()[0].finish_reason(),
        FinishReason::Stop,
        "the last finish event is the folded reason"
    );
    assert_eq!(
        response
            .usage()
            .reported_counts()
            .and_then(|counts| counts.get(TokenKind::Output)),
        Some(2),
        "the last usage report wins"
    );
}
