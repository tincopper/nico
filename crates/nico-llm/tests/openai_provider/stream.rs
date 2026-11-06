//! Decoding the Chat Completions event stream into canonical events.

use nico_core::ErrorCategory;
use nico_llm::{
    FinishReason, ModelId, Provider, ProviderError, ResponseId, StreamEvent, TokenKind, Usage,
    UsageCounts, provider::openai::MAX_TOOL_CALLS,
};
use serde_json::{Value, json};

use crate::support::{done, events, events_at, finish_err, frame, provider, push_err};

mod tool_calls;

/// One chunk frame.
fn chunk(value: Value) -> String {
    frame(&value.to_string())
}

/// The chunk that announces the response identity and the assistant role.
fn start() -> String {
    chunk(json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": "chat-small-2026-01-01",
        "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": null}],
    }))
}

/// A chunk carrying a text delta.
fn text(value: &str) -> String {
    chunk(json!({
        "choices": [{"index": 0, "delta": {"content": value}, "finish_reason": null}],
    }))
}

/// A chunk carrying a reasoning delta.
fn reasoning(value: &str) -> String {
    chunk(json!({
        "choices": [{"index": 0, "delta": {"reasoning_content": value}, "finish_reason": null}],
    }))
}

/// A chunk carrying a tool-call fragment.
fn tool_call(value: Value) -> String {
    chunk(json!({
        "choices": [{"index": 0, "delta": {"tool_calls": [value]}, "finish_reason": null}],
    }))
}

/// The chunk that reports why generation stopped.
fn finish(reason: &str) -> String {
    chunk(json!({
        "choices": [{"index": 0, "delta": {}, "finish_reason": reason}],
    }))
}

/// A usage-only chunk.
fn usage() -> String {
    chunk(json!({
        "choices": [],
        "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
    }))
}

/// The identity every fixture stream announces.
fn identity() -> (ResponseId, ModelId) {
    (
        ResponseId::try_new("chatcmpl-1").expect("a valid id"),
        ModelId::try_new("chat-small-2026-01-01").expect("a valid model"),
    )
}

/// The canonical start event.
fn start_event() -> StreamEvent {
    let (id, model) = identity();
    StreamEvent::Start { id, model }
}

/// The reported usage of the fixture stream.
fn reported() -> Usage {
    Usage::reported(
        UsageCounts::new()
            .with(TokenKind::Input, 3)
            .with(TokenKind::Output, 2),
    )
}

#[test]
fn a_text_stream_decodes_to_the_canonical_sequence() {
    let events = events(&[
        start(),
        text("Hel"),
        text("lo"),
        finish("stop"),
        usage(),
        done(),
    ]);
    assert_eq!(
        events,
        vec![
            start_event(),
            StreamEvent::TextDelta("Hel".to_owned()),
            StreamEvent::TextDelta("lo".to_owned()),
            StreamEvent::Finish(FinishReason::Stop),
            StreamEvent::Usage(reported()),
        ]
    );
}

#[test]
fn usage_may_arrive_before_the_finish_reason() {
    let events = events(&[start(), text("hi"), usage(), finish("length"), done()]);
    assert_eq!(
        events,
        vec![
            start_event(),
            StreamEvent::TextDelta("hi".to_owned()),
            StreamEvent::Usage(reported()),
            StreamEvent::Finish(FinishReason::Length),
        ]
    );
}

#[test]
fn any_fragmentation_produces_the_same_sequence() {
    let chunks = vec![
        start(),
        text("a"),
        reasoning("think"),
        finish("stop"),
        done(),
    ];
    let expected = events(&chunks);
    let joined = chunks.concat();
    for size in 1..=joined.len() {
        assert_eq!(
            events_at(&chunks, size),
            expected,
            "chunk size {size} changed the event sequence"
        );
    }
}

#[test]
fn a_terminator_without_a_finish_reason_is_refused() {
    let error = finish_err(&[start(), text("hi"), done()]);
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
}

#[test]
fn a_body_that_ends_after_a_finish_reason_is_complete_without_the_sentinel() {
    // The finish reason is what makes a stream complete; `[DONE]` is the protocol's redundant
    // terminator, and compatible endpoints do omit it. The client encoder writes its own terminator
    // at the end of the body.
    let events = events(&[start(), text("hi"), finish("stop")]);
    assert_eq!(
        events,
        vec![
            start_event(),
            StreamEvent::TextDelta("hi".to_owned()),
            StreamEvent::Finish(FinishReason::Stop),
        ]
    );
}

#[test]
fn a_body_that_ends_before_a_finish_reason_is_refused() {
    let error = finish_err(&[start(), text("hi")]);
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "the client must not be told the model finished when nobody said it did: {error:?}"
    );
}

#[test]
fn a_stream_that_never_announces_its_response_is_refused() {
    let error = push_err(&[text("hi")]);
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
}

#[test]
fn a_partial_identity_is_refused() {
    let partial = chunk(json!({
        "id": "chatcmpl-1",
        "choices": [{"index": 0, "delta": {"content": "hi"}, "finish_reason": null}],
    }));
    let error = push_err(&[partial]);
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
}

#[test]
fn a_changed_identity_is_refused() {
    let changed = chunk(json!({
        "id": "chatcmpl-2",
        "model": "other-model",
        "choices": [{"index": 0, "delta": {"content": "hi"}, "finish_reason": null}],
    }));
    let error = push_err(&[start(), changed]);
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
}

#[test]
fn a_later_chunk_may_repeat_only_part_of_the_identity() {
    // A missing field is not a contradiction: the stream already announced both parts, and a chunk
    // that repeats one of them is still the same response.
    let repeated = chunk(json!({
        "model": "chat-small-2026-01-01",
        "choices": [{"index": 0, "delta": {"content": "hi"}, "finish_reason": null}],
    }));
    assert_eq!(
        events(&[start(), repeated, finish("stop"), done()]),
        vec![
            start_event(),
            StreamEvent::TextDelta("hi".to_owned()),
            StreamEvent::Finish(FinishReason::Stop),
        ]
    );
}

#[test]
fn a_repeated_identity_that_disagrees_is_refused() {
    let changed = chunk(json!({
        "id": "chatcmpl-2",
        "choices": [{"index": 0, "delta": {"content": "hi"}, "finish_reason": null}],
    }));
    let error = push_err(&[start(), changed]);
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
}

#[test]
fn content_after_the_finish_reason_is_refused() {
    let error = push_err(&[start(), finish("stop"), text("late")]);
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
}

#[test]
fn a_finish_reason_reported_twice_is_refused() {
    let error = push_err(&[start(), finish("stop"), finish("stop")]);
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
}

#[test]
fn a_second_choice_is_refused() {
    let second = chunk(json!({
        "choices": [{"index": 1, "delta": {"content": "hi"}, "finish_reason": null}],
    }));
    let error = push_err(&[start(), second]);
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
}

#[test]
fn an_error_frame_becomes_a_stream_error_and_ends_the_decoder() {
    let failed = chunk(json!({
        "error": {
            "message": "secret upstream detail",
            "type": "server_error",
            "param": null,
            "code": "provider_x",
        },
    }));
    let events = events(&[start(), failed, text("ignored"), done()]);
    assert_eq!(events.len(), 2, "nothing follows the error: {events:?}");
    assert_eq!(events[0], start_event());
    let StreamEvent::Error(error) = &events[1] else {
        panic!("the second event must be the failure");
    };
    error.validate().expect("a mapped stream error is valid");
    assert_eq!(error.category(), ErrorCategory::Upstream);
    assert_eq!(error.code(), "upstream_error");
    assert_eq!(error.http_status(), 502);
    assert!(!error.retryable(), "a byte already reached the caller");
    assert_eq!(error.provider_code(), Some("provider_x"));
    assert!(
        !error.message().contains("secret upstream detail"),
        "the provider's message must not reach the caller"
    );
}

#[test]
fn a_broken_frame_is_refused_without_quoting_it() {
    let broken = frame("{\"model\":\"MARKER\",\"choices\":[");
    let error = push_err(&[start(), broken]);
    let record = format!("{error:?} {error}");
    assert!(matches!(error, ProviderError::Malformed { .. }), "{record}");
    assert!(
        !record.contains("MARKER"),
        "a diagnostic must not quote the provider's bytes: {record}"
    );
}

#[test]
fn a_truncated_frame_is_refused() {
    // The frame never gets its blank line, so the end of the body finds it incomplete.
    let error = finish_err(&[start(), "data: {\"choices\":[]}".to_owned()]);
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
}

#[test]
fn heartbeats_and_empty_frames_are_ignored() {
    let events = events(&[
        ": ping\n\n".to_owned(),
        start(),
        "data: \n\n".to_owned(),
        text("hi"),
        "\n".to_owned(),
        finish("stop"),
        done(),
    ]);
    assert_eq!(
        events,
        vec![
            start_event(),
            StreamEvent::TextDelta("hi".to_owned()),
            StreamEvent::Finish(FinishReason::Stop),
        ]
    );
}

#[test]
fn an_unknown_chunk_field_is_ignored() {
    let annotated = chunk(json!({
        "system_fingerprint": "fp_1",
        "choices": [{"index": 0, "delta": {"content": "hi"}, "finish_reason": null}],
    }));
    assert_eq!(
        events(&[start(), annotated, finish("stop"), done()]),
        vec![
            start_event(),
            StreamEvent::TextDelta("hi".to_owned()),
            StreamEvent::Finish(FinishReason::Stop),
        ]
    );
}

#[test]
fn a_framing_field_the_decoder_ignores_cannot_fail_the_document() {
    // `object` and `created` are consumed without inspection, so a chunk that types them differently
    // still carries a readable delta.
    let odd = chunk(json!({
        "id": "chatcmpl-1",
        "model": "chat-small-2026-01-01",
        "object": 7,
        "created": "yesterday",
        "choices": [{"index": 0, "delta": {"content": "hi"}, "finish_reason": null}],
    }));
    assert_eq!(
        events(&[odd, finish("stop"), done()]),
        vec![
            start_event(),
            StreamEvent::TextDelta("hi".to_owned()),
            StreamEvent::Finish(FinishReason::Stop),
        ]
    );
}

#[test]
fn a_partial_frame_dies_with_its_response() {
    // The first response ends in the middle of a frame; a decoder created for the next response must
    // not inherit those bytes, or one answer could complete another one's event.
    let mut abandoned = provider().stream_decoder();
    abandoned
        .push(b"data: {\"id\":")
        .expect("a partial frame is buffered, not refused");
    let events = events(&[start(), text("hi"), finish("stop"), done()]);
    assert_eq!(events[1], StreamEvent::TextDelta("hi".to_owned()));
}

#[test]
fn more_open_tool_calls_than_the_bound_is_refused() {
    // Every fragment names its own `index`, so the number of open calls is the peer's choice and the
    // table is retained for the life of the response. The adapter caps it rather than growing on
    // demand; the first fragment past the bound is what reports it.
    let mut chunks = vec![start()];
    for index in 0..=MAX_TOOL_CALLS {
        chunks.push(tool_call(json!({
            "index": index,
            "id": format!("call_{index}"),
            "type": "function",
            "function": {"name": "lookup", "arguments": ""},
        })));
    }

    let error = push_err(&chunks);
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "expected Malformed, got {error:?}"
    );
}
