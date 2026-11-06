//! Decoding the Chat Completions response document into canonical values.

use nico_core::ErrorCategory;
use nico_llm::{
    FinishReason, ProviderError, TokenKind,
    schema::{AssistantTurn, ChatResponse},
};
use serde_json::{Value, json};

use crate::support::{decode_err, decoded};

/// Builds a complete answer with one choice.
fn answer(choice: Value) -> String {
    json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "created": 1_700_000_000,
        "model": "chat-small-2026-01-01",
        "choices": [choice],
    })
    .to_string()
}

/// Builds one choice with a text message.
fn text_choice(content: &str, finish_reason: &str) -> Value {
    json!({
        "index": 0,
        "message": {"role": "assistant", "content": content},
        "finish_reason": finish_reason,
    })
}

/// Returns the answer's first turn.
fn turn(response: &ChatResponse) -> &AssistantTurn {
    response.choices()[0].turn()
}

#[test]
fn a_text_answer_is_decoded() {
    let response = decoded(&answer(text_choice("hello", "stop")));
    assert_eq!(response.id().as_str(), "chatcmpl-1");
    assert_eq!(response.model().as_str(), "chat-small-2026-01-01");
    assert_eq!(response.choices().len(), 1);
    assert_eq!(response.choices()[0].index(), 0);
    assert_eq!(response.choices()[0].finish_reason(), FinishReason::Stop);
    assert_eq!(
        turn(&response).content()[0],
        nico_llm::ContentPart::text("hello")
    );
    assert!(turn(&response).tool_calls().is_empty());
}

#[test]
fn every_finish_reason_is_mapped() {
    for (wire, reason) in [
        ("stop", FinishReason::Stop),
        ("length", FinishReason::Length),
        ("tool_calls", FinishReason::ToolCalls),
        ("function_call", FinishReason::ToolCalls),
        ("content_filter", FinishReason::ContentFilter),
    ] {
        let response = decoded(&answer(text_choice("x", wire)));
        assert_eq!(
            response.choices()[0].finish_reason(),
            reason,
            "wire reason {wire}"
        );
    }
}

#[test]
fn tool_calls_are_decoded_with_json_arguments() {
    let choice = json!({
        "index": 0,
        "message": {
            "role": "assistant",
            "content": null,
            "tool_calls": [
                {
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "lookup", "arguments": "{\"q\":\"x\"}"},
                },
                {
                    "id": "call_2",
                    "type": "function",
                    "function": {"name": "fetch", "arguments": "{}"},
                },
            ],
        },
        "finish_reason": "tool_calls",
    });
    let response = decoded(&answer(choice));
    let calls = turn(&response).tool_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].id().as_str(), "call_1");
    assert_eq!(calls[0].name().as_str(), "lookup");
    assert_eq!(calls[0].arguments().as_value(), &json!({"q": "x"}));
    assert_eq!(calls[1].name().as_str(), "fetch");
    assert!(turn(&response).content().is_empty());
}

#[test]
fn empty_tool_arguments_become_an_empty_object() {
    let choice = json!({
        "index": 0,
        "message": {
            "role": "assistant",
            "content": null,
            "tool_calls": [
                {"id": "call_1", "type": "function", "function": {"name": "ping", "arguments": ""}},
            ],
        },
        "finish_reason": "tool_calls",
    });
    let response = decoded(&answer(choice));
    assert_eq!(
        turn(&response).tool_calls()[0].arguments().as_value(),
        &json!({})
    );
}

#[test]
fn reasoning_content_becomes_a_reasoning_item() {
    let choice = json!({
        "index": 0,
        "message": {"role": "assistant", "content": "answer", "reasoning_content": "think"},
        "finish_reason": "stop",
    });
    let response = decoded(&answer(choice));
    let reasoning = turn(&response)
        .reasoning()
        .expect("the turn carries reasoning");
    assert_eq!(reasoning.text(), "think");
    assert_eq!(reasoning.signature(), None);
}

#[test]
fn an_empty_reasoning_field_is_not_a_reasoning_item() {
    let choice = json!({
        "index": 0,
        "message": {"role": "assistant", "content": "answer", "reasoning_content": ""},
        "finish_reason": "stop",
    });
    assert_eq!(turn(&decoded(&answer(choice))).reasoning(), None);
}

#[test]
fn usage_keeps_every_reported_count() {
    let body = json!({
        "id": "chatcmpl-1",
        "model": "chat-small-2026-01-01",
        "choices": [text_choice("hello", "stop")],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 5,
            "total_tokens": 15,
            "prompt_tokens_details": {"cached_tokens": 3},
            "completion_tokens_details": {"reasoning_tokens": 2},
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
    assert_eq!(reported.get(TokenKind::Reasoning), Some(2));
    assert_eq!(reported.get(TokenKind::CacheWrite), None);
    assert_eq!(
        response.usage().measured_counts(),
        None,
        "the adapter never fabricates a measured count"
    );
}

#[test]
fn a_partial_usage_stays_partial() {
    let body = json!({
        "id": "chatcmpl-1",
        "model": "chat-small-2026-01-01",
        "choices": [text_choice("hello", "stop")],
        "usage": {"prompt_tokens": 10},
    })
    .to_string();
    let response = decoded(&body);
    let reported = response
        .usage()
        .reported_counts()
        .expect("a reported count");
    assert_eq!(reported.get(TokenKind::Input), Some(10));
    assert_eq!(reported.get(TokenKind::Output), None);
}

#[test]
fn a_response_without_usage_has_no_bucket() {
    assert_eq!(
        decoded(&answer(text_choice("hello", "stop")))
            .usage()
            .reported_counts(),
        None
    );
}

#[test]
fn multiple_choices_keep_their_indexes() {
    let body = json!({
        "id": "chatcmpl-1",
        "model": "chat-small-2026-01-01",
        "choices": [
            {"index": 0, "message": {"role": "assistant", "content": "first"}, "finish_reason": "stop"},
            {"index": 1, "message": {"role": "assistant", "content": "second"}, "finish_reason": "length"},
        ],
    })
    .to_string();
    let response = decoded(&body);
    assert_eq!(response.choices().len(), 2);
    assert_eq!(response.choices()[0].index(), 0);
    assert_eq!(response.choices()[1].index(), 1);
    assert_eq!(response.choices()[1].finish_reason(), FinishReason::Length);
}

#[test]
fn framing_fields_are_consumed_and_unknown_ones_are_preserved() {
    let body = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "created": 1_700_000_000,
        "model": "chat-small-2026-01-01",
        "choices": [text_choice("hello", "stop")],
        "system_fingerprint": "fp_1",
        "service_tier": "default",
    })
    .to_string();
    let response = decoded(&body);
    assert_eq!(
        response.extensions().get("system_fingerprint"),
        Some(&nico_llm::RawJson::new(json!("fp_1")))
    );
    assert_eq!(
        response.extensions().get("service_tier"),
        Some(&nico_llm::RawJson::new(json!("default")))
    );
    assert!(
        response.extensions().get("object").is_none(),
        "the object field belongs to this document and must not shadow a client field"
    );
    assert!(response.extensions().get("created").is_none());
}

#[test]
fn a_framing_field_the_adapter_ignores_cannot_fail_the_document() {
    // `created` is consumed without inspection: a provider that types it differently says nothing
    // about the answer, and a field this adapter never reads must not refuse a response it can serve.
    let body = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "created": "2026-01-01T00:00:00Z",
        "model": "chat-small-2026-01-01",
        "choices": [text_choice("hello", "stop")],
    })
    .to_string();
    let response = decoded(&body);
    assert_eq!(response.choices().len(), 1);
    assert!(response.extensions().get("created").is_none());
}

#[test]
fn an_answer_that_is_not_a_completion_is_refused() {
    let body = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "model": "chat-small-2026-01-01",
        "choices": [text_choice("hello", "stop")],
    })
    .to_string();
    assert!(matches!(
        decode_err(200, &body),
        ProviderError::Malformed { .. }
    ));
}

#[test]
fn a_broken_document_is_refused() {
    for body in [
        "not json".to_owned(),
        answer(json!({
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "lookup", "arguments": "not json"}},
                ],
            },
            "finish_reason": "tool_calls",
        })),
        json!({"id": "chatcmpl-1", "model": "m"}).to_string(),
        json!({
            "id": "",
            "model": "m",
            "choices": [text_choice("hello", "stop")],
        })
        .to_string(),
        json!({
            "id": "chatcmpl-1",
            "model": "m",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "x"}, "finish_reason": "unknown"}],
        })
        .to_string(),
        json!({
            "id": "chatcmpl-1",
            "model": "m",
            "choices": [],
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
fn an_unprintable_unknown_field_is_refused_without_being_quoted() {
    // A member name is the provider's text as well; the refusal still refuses the document but does
    // not repeat a name that could forge a second log record.
    let choice = json!({
        "index": 0,
        "message": {"role": "assistant", "content": "x", "evil\nkey": "MARKER"},
        "finish_reason": "stop",
    });
    let ProviderError::Malformed { reason, .. } = decode_err(200, &answer(choice)) else {
        panic!("a document with an unmodelled member must be refused");
    };
    assert!(!reason.contains("evil"), "the name is not quoted: {reason}");
    assert!(
        reason.contains("does not model"),
        "the refusal still says what broke: {reason}"
    );
}

#[test]
fn an_unknown_field_inside_the_answer_tree_is_refused() {
    for choice in [
        json!({
            "index": 0,
            "message": {"role": "assistant", "content": "x", "refusal": "no"},
            "finish_reason": "stop",
        }),
        json!({
            "index": 0,
            "message": {"role": "assistant", "content": "x"},
            "finish_reason": "stop",
            "logprobs": {"content": []},
        }),
        json!({
            "index": 0,
            "message": {"role": "assistant", "content": "x", "unknown": 1},
            "finish_reason": "stop",
        }),
    ] {
        assert!(matches!(
            decode_err(200, &answer(choice)),
            ProviderError::Malformed { .. }
        ));
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
        (503, ErrorCategory::Upstream, "upstream_error", 502, true),
        (302, ErrorCategory::Upstream, "upstream_error", 502, false),
    ] {
        let body = json!({
            "error": {
                "message": "secret upstream detail",
                "type": "invalid_request_error",
                "code": "provider_x",
            }
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
            Some("provider_x"),
            "the provider code is kept for the log"
        );
    }
}

#[test]
fn an_unusable_provider_code_is_ignored() {
    for body in [
        json!({"error": {"code": 500}}).to_string(),
        json!({"error": {"code": null}}).to_string(),
        json!({"error": {"code": ""}}).to_string(),
        json!({"error": {}}).to_string(),
        // The code reaches a log field, never an answer, so a value that could shape the record is
        // dropped rather than sanitized.
        json!({"error": {"code": "two\nlines"}}).to_string(),
        json!({"error": {"code": "   "}}).to_string(),
        json!({"error": {"code": "限流"}}).to_string(),
        json!({"error": {"code": "a".repeat(129)}}).to_string(),
        "not json".to_owned(),
    ] {
        let ProviderError::Upstream { error } = decode_err(400, &body) else {
            panic!("a failure status must be an upstream error");
        };
        assert_eq!(error.provider_code(), None, "body: {body}");
    }
}

#[test]
fn an_oversized_error_body_is_not_scanned() {
    let body = json!({
        "error": {"code": "provider_x"},
        "padding": "a".repeat(70 * 1024),
    })
    .to_string();
    let ProviderError::Upstream { error } = decode_err(500, &body) else {
        panic!("a failure status must be an upstream error");
    };
    assert_eq!(
        error.provider_code(),
        None,
        "a body that large is not an error envelope worth scanning"
    );
    assert_eq!(error.code(), "upstream_error");
}
