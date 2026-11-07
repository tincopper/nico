//! Decoding the Messages response document into canonical values.

use nico_core::ErrorCategory;
use nico_llm::{
    FinishReason, Provider, ProviderError, TokenKind,
    schema::{AssistantTurn, ChatResponse},
};
use serde_json::{Value, json};

use crate::support::{decode_err, decoded, provider};

/// Builds a complete answer with the given content blocks and stop reason.
fn answer(content: Value, stop_reason: &str) -> String {
    json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "claude-small-2026-01-01",
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {"input_tokens": 10, "output_tokens": 5},
    })
    .to_string()
}

/// Builds a text-only answer.
fn text_answer(text: &str, stop_reason: &str) -> String {
    answer(json!([{"type": "text", "text": text}]), stop_reason)
}

/// Returns the answer's first turn.
fn turn(response: &ChatResponse) -> &AssistantTurn {
    response.choices()[0].turn()
}

#[test]
fn a_text_answer_is_decoded() {
    let response = decoded(&text_answer("hello", "end_turn"));
    assert_eq!(response.id().as_str(), "msg_1");
    assert_eq!(response.model().as_str(), "claude-small-2026-01-01");
    assert_eq!(response.choices().len(), 1);
    assert_eq!(response.choices()[0].index(), 0);
    assert_eq!(response.choices()[0].finish_reason(), FinishReason::Stop);
    assert_eq!(
        turn(&response).content()[0],
        nico_llm::ContentPart::text("hello")
    );
    assert!(turn(&response).tool_calls().is_empty());
    assert!(turn(&response).reasoning().is_none());
}

#[test]
fn every_finish_reason_is_mapped() {
    for (wire, reason) in [
        ("end_turn", FinishReason::Stop),
        ("stop_sequence", FinishReason::Stop),
        ("max_tokens", FinishReason::Length),
        ("tool_use", FinishReason::ToolCalls),
        ("refusal", FinishReason::ContentFilter),
    ] {
        let response = decoded(&text_answer("x", wire));
        assert_eq!(
            response.choices()[0].finish_reason(),
            reason,
            "wire reason {wire}"
        );
    }
}

#[test]
fn a_finish_reason_the_model_has_no_value_for_is_refused() {
    // `pause_turn` means the server-side tool loop paused and the caller should continue it; this
    // gateway has no server tools, and calling that `stop` would tell the caller the model finished.
    for wire in ["pause_turn", "unknown", ""] {
        assert!(
            matches!(
                decode_err(200, &text_answer("x", wire)),
                ProviderError::Malformed { .. }
            ),
            "wire reason `{wire}` must be refused"
        );
    }
}

#[test]
fn a_missing_stop_reason_is_refused() {
    let body = json!({
        "id": "msg_1",
        "model": "claude-small-2026-01-01",
        "content": [{"type": "text", "text": "x"}],
    })
    .to_string();
    assert!(matches!(
        decode_err(200, &body),
        ProviderError::Malformed { .. }
    ));
}

#[test]
fn tool_calls_are_decoded_with_object_arguments() {
    let body = answer(
        json!([
            {"type": "text", "text": "let me check"},
            {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {"q": "x"}},
            {"type": "tool_use", "id": "toolu_2", "name": "fetch", "input": {}},
        ]),
        "tool_use",
    );
    let response = decoded(&body);
    assert_eq!(
        response.choices()[0].finish_reason(),
        FinishReason::ToolCalls
    );
    let calls = turn(&response).tool_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].id().as_str(), "toolu_1");
    assert_eq!(calls[0].name().as_str(), "lookup");
    assert_eq!(calls[0].arguments().as_value(), &json!({"q": "x"}));
    assert_eq!(calls[1].name().as_str(), "fetch");
    assert_eq!(turn(&response).content().len(), 1);
}

#[test]
fn a_thinking_block_becomes_a_signed_reasoning_item() {
    // A response may carry a thinking block even though this adapter cannot send one back; refusing
    // to read it would hide part of the answer the model produced.
    let body = answer(
        json!([
            {"type": "thinking", "thinking": "consider", "signature": "sig-1"},
            {"type": "text", "text": "answer"},
        ]),
        "end_turn",
    );
    let response = decoded(&body);
    let reasoning = turn(&response)
        .reasoning()
        .expect("the turn carries reasoning");
    assert_eq!(reasoning.text(), "consider");
    assert_eq!(reasoning.signature(), Some("sig-1"));
}

#[test]
fn a_thinking_block_without_a_signature_is_still_reasoning() {
    let body = answer(
        json!([{"type": "thinking", "thinking": "consider", "signature": ""}]),
        "end_turn",
    );
    assert_eq!(
        turn(&decoded(&body))
            .reasoning()
            .expect("the turn carries reasoning")
            .signature(),
        None,
        "an empty signature is the absence of one"
    );
}

#[test]
fn a_second_thinking_block_is_refused() {
    let body = answer(
        json!([
            {"type": "thinking", "thinking": "one", "signature": "sig-1"},
            {"type": "thinking", "thinking": "two", "signature": "sig-2"},
        ]),
        "end_turn",
    );
    assert!(
        matches!(decode_err(200, &body), ProviderError::Malformed { .. }),
        "the canonical turn carries one reasoning value, so a second is not merged silently"
    );
}

#[test]
fn an_empty_text_block_is_refused() {
    let body = answer(json!([{"type": "text", "text": ""}]), "end_turn");
    assert!(matches!(
        decode_err(200, &body),
        ProviderError::Malformed { .. }
    ));
}

#[test]
fn usage_keeps_every_reported_count() {
    let body = json!({
        "id": "msg_1",
        "model": "claude-small-2026-01-01",
        "content": [{"type": "text", "text": "hello"}],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 10,
            "output_tokens": 5,
            "cache_read_input_tokens": 3,
            "cache_creation_input_tokens": 2,
        },
    })
    .to_string();
    let response = decoded(&body);
    let reported = response
        .usage()
        .reported_counts()
        .expect("the provider reported usage");
    assert_eq!(reported.get(TokenKind::Input), Some(10));
    assert_eq!(reported.get(TokenKind::Output), Some(5));
    assert_eq!(reported.get(TokenKind::CacheRead), Some(3));
    assert_eq!(reported.get(TokenKind::CacheWrite), Some(2));
    assert_eq!(reported.get(TokenKind::Reasoning), None);
    assert_eq!(
        response.usage().measured_counts(),
        None,
        "the adapter never fabricates a measured count"
    );
}

#[test]
fn a_partial_usage_stays_partial() {
    let body = json!({
        "id": "msg_1",
        "model": "claude-small-2026-01-01",
        "content": [{"type": "text", "text": "hello"}],
        "stop_reason": "end_turn",
        "usage": {"output_tokens": 5},
    })
    .to_string();
    let response = decoded(&body);
    let reported = response
        .usage()
        .reported_counts()
        .expect("a reported count");
    assert_eq!(reported.get(TokenKind::Input), None);
    assert_eq!(reported.get(TokenKind::Output), Some(5));
}

#[test]
fn a_response_without_usage_has_no_bucket() {
    let body = json!({
        "id": "msg_1",
        "model": "claude-small-2026-01-01",
        "content": [{"type": "text", "text": "hello"}],
        "stop_reason": "end_turn",
    })
    .to_string();
    assert_eq!(decoded(&body).usage().reported_counts(), None);
}

#[test]
fn framing_fields_are_consumed_and_unknown_ones_are_preserved() {
    let body = json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "claude-small-2026-01-01",
        "content": [{"type": "text", "text": "hello"}],
        "stop_reason": "stop_sequence",
        "stop_sequence": "MARKER",
        "request_id": "req_1",
    })
    .to_string();
    let response = decoded(&body);
    assert_eq!(
        response.extensions().get("request_id"),
        Some(&nico_llm::RawJson::new(json!("req_1")))
    );
    assert!(
        response.extensions().get("stop_sequence").is_none(),
        "the matched stop string is the caller's own text and belongs to no client field"
    );
    assert!(response.extensions().get("type").is_none());
    assert!(response.extensions().get("role").is_none());
    assert_eq!(
        response.choices()[0].finish_reason(),
        FinishReason::Stop,
        "the reason is what the canonical model keeps"
    );
}

#[test]
fn an_answer_that_is_not_a_message_is_refused() {
    let body = json!({
        "id": "msg_1",
        "type": "message_start",
        "role": "assistant",
        "model": "claude-small-2026-01-01",
        "content": [{"type": "text", "text": "hello"}],
        "stop_reason": "end_turn",
    })
    .to_string();
    assert!(matches!(
        decode_err(200, &body),
        ProviderError::Malformed { .. }
    ));
}

#[test]
fn a_non_assistant_message_is_refused() {
    let body = json!({
        "id": "msg_1",
        "role": "user",
        "model": "claude-small-2026-01-01",
        "content": [{"type": "text", "text": "hello"}],
        "stop_reason": "end_turn",
    })
    .to_string();
    assert!(matches!(
        decode_err(200, &body),
        ProviderError::Malformed { .. }
    ));
}

#[test]
fn a_block_this_adapter_cannot_express_is_refused() {
    for block in [
        json!({"type": "redacted_thinking", "data": "opaque"}),
        json!({"type": "server_tool_use", "id": "srv_1", "name": "web_search"}),
        json!({"type": "text", "text": "x", "citations": [{"cited_text": "q"}]}),
        json!({"type": "text", "text": "x", "cache_control": {"type": "ephemeral"}}),
    ] {
        assert!(
            matches!(
                decode_err(200, &answer(json!([block]), "end_turn")),
                ProviderError::Malformed { .. }
            ),
            "the block must be refused rather than skipped"
        );
    }
}

#[test]
fn an_unprintable_unknown_field_is_refused_without_being_quoted() {
    let body = answer(
        json!([{"type": "text", "text": "x", "evil\nkey": "MARKER"}]),
        "end_turn",
    );
    let ProviderError::Malformed { reason, .. } = decode_err(200, &body) else {
        panic!("a document with an unmodelled member must be refused");
    };
    assert!(!reason.contains("evil"), "the name is not quoted: {reason}");
    assert!(
        reason.contains("does not model"),
        "the refusal still says what broke: {reason}"
    );
}

#[test]
fn a_broken_document_is_refused() {
    for body in [
        "not json".to_owned(),
        json!({"id": "msg_1", "model": "m"}).to_string(),
        json!({
            "id": "",
            "model": "m",
            "content": [{"type": "text", "text": "x"}],
            "stop_reason": "end_turn",
        })
        .to_string(),
        json!({
            "id": "msg_1",
            "model": "m",
            "content": [{"type": "tool_use", "id": "toolu_1", "name": "lookup"}],
            "stop_reason": "tool_use",
        })
        .to_string(),
        json!({
            "id": "msg_1",
            "model": "m",
            "content": [{"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": [1]}],
            "stop_reason": "tool_use",
        })
        .to_string(),
    ] {
        assert!(
            matches!(decode_err(200, &body), ProviderError::Malformed { .. }),
            "the document must be refused: {body}"
        );
    }
}

#[test]
fn a_failure_status_maps_to_the_canonical_error_table() {
    for (status, category, code, caller_status, retryable) in [
        (400, ErrorCategory::Client, "upstream_rejected", 400, false),
        (
            401,
            ErrorCategory::Upstream,
            "upstream_auth_failed",
            502,
            false,
        ),
        (
            403,
            ErrorCategory::Upstream,
            "upstream_auth_failed",
            502,
            false,
        ),
        (
            404,
            ErrorCategory::Client,
            "upstream_model_not_found",
            404,
            false,
        ),
        (408, ErrorCategory::Upstream, "upstream_timeout", 504, true),
        (
            429,
            ErrorCategory::Limit,
            "upstream_rate_limited",
            429,
            true,
        ),
        (422, ErrorCategory::Client, "upstream_rejected", 422, false),
        (500, ErrorCategory::Upstream, "upstream_error", 502, true),
        (529, ErrorCategory::Upstream, "upstream_error", 502, true),
        (302, ErrorCategory::Upstream, "upstream_error", 502, false),
    ] {
        let body = json!({
            "type": "error",
            "error": {
                "type": "overloaded_error",
                "message": "secret upstream detail",
            },
        })
        .to_string();
        let ProviderError::Upstream { error } = decode_err(status, &body) else {
            panic!("a failure status must be an upstream error");
        };
        error.validate().expect("a mapped error is valid");
        assert_eq!(error.category(), category, "category of {status}");
        assert_eq!(error.code(), code, "code of {status}");
        assert_eq!(
            error.http_status(),
            caller_status,
            "caller status of {status}"
        );
        assert_eq!(error.retryable(), retryable, "retryable of {status}");
        assert!(
            !error.message().contains("secret upstream detail"),
            "the provider's message must not reach the caller"
        );
        assert_eq!(
            error.provider_code(),
            Some("overloaded_error"),
            "the provider's own type is kept for the log"
        );
    }
}

#[test]
fn an_unusable_provider_type_is_ignored() {
    for body in [
        json!({"error": {"type": 500}}).to_string(),
        json!({"error": {"type": null}}).to_string(),
        json!({"error": {"type": ""}}).to_string(),
        json!({"error": {}}).to_string(),
        // The type reaches a log field, never an answer, so a value that could shape the record is
        // dropped rather than sanitized.
        json!({"error": {"type": "two\nlines"}}).to_string(),
        json!({"error": {"type": "限流"}}).to_string(),
        json!({"error": {"type": "a".repeat(129)}}).to_string(),
        "not json".to_owned(),
    ] {
        let ProviderError::Upstream { error } = decode_err(400, &body) else {
            panic!("a failure status must be an upstream error");
        };
        assert_eq!(error.provider_code(), None, "body: {body}");
    }
}

#[test]
fn the_two_status_paths_agree_about_a_failure() {
    // `head` writes the client status before the body is read; `decode_response` reads the body
    // afterwards. A status answered differently by the two would put a body under a head that
    // disagrees with it.
    for status in [400, 401, 403, 404, 408, 418, 429, 500, 529] {
        let declared = provider().error_for_status(status);
        let ProviderError::Upstream { error } =
            decode_err(status, r#"{"error":{"type":"provider_marker"}}"#)
        else {
            panic!("{status} must be an upstream failure");
        };
        assert_eq!(declared.category(), error.category(), "{status}");
        assert_eq!(declared.code(), error.code(), "{status}");
        assert_eq!(declared.http_status(), error.http_status(), "{status}");
        assert_eq!(declared.retryable(), error.retryable(), "{status}");
    }
}

#[test]
fn an_oversized_error_body_is_not_scanned() {
    let body = json!({
        "error": {"type": "overloaded_error"},
        "padding": "a".repeat(70 * 1024),
    })
    .to_string();
    let ProviderError::Upstream { error } = decode_err(500, &body) else {
        panic!("a failure status must be an upstream error");
    };
    assert_eq!(error.provider_code(), None);
    assert_eq!(error.code(), "upstream_error");
}
