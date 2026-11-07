//! Decoding the Messages event stream into canonical events.

use nico_llm::{
    FinishReason, ModelId, ProviderError, ResponseId, StreamEvent, TokenKind, Usage, UsageCounts,
};
use serde_json::json;

use crate::support::{event, events, events_at, json_event, push_err};

mod framing;
mod lifecycle;
mod refusals;
mod tool_calls;

/// The event that announces the response identity and the prompt-side usage.
fn start() -> String {
    json_event(
        "message_start",
        json!({
            "type": "message_start",
            "message": {
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "model": "claude-small-2026-01-01",
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {"input_tokens": 10, "output_tokens": 1},
            },
        }),
    )
}

/// The event that opens a text block.
fn text_block(index: u32) -> String {
    json_event(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": index,
            "content_block": {"type": "text", "text": ""},
        }),
    )
}

/// A text increment.
fn text(index: u32, value: &str) -> String {
    json_event(
        "content_block_delta",
        json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "text_delta", "text": value},
        }),
    )
}

/// The event that closes a block.
fn block_stop(index: u32) -> String {
    json_event(
        "content_block_stop",
        json!({"type": "content_block_stop", "index": index}),
    )
}

/// The event that reports why generation stopped and the completion-side usage.
fn finish(reason: &str) -> String {
    json_event(
        "message_delta",
        json!({
            "type": "message_delta",
            "delta": {"stop_reason": reason, "stop_sequence": null},
            "usage": {"output_tokens": 5},
        }),
    )
}

/// A usage-only message delta.
fn usage_only() -> String {
    json_event(
        "message_delta",
        json!({"type": "message_delta", "delta": {}, "usage": {"output_tokens": 4}}),
    )
}

/// The stream terminator.
fn stop() -> String {
    json_event("message_stop", json!({"type": "message_stop"}))
}

/// The identity every fixture stream announces.
fn identity() -> (ResponseId, ModelId) {
    (
        ResponseId::try_new("msg_1").expect("a valid id"),
        ModelId::try_new("claude-small-2026-01-01").expect("a valid model"),
    )
}

/// The canonical start event.
fn start_event() -> StreamEvent {
    let (id, model) = identity();
    StreamEvent::Start { id, model }
}

/// The prompt-side usage `message_start` reports.
fn reported(input: u64, output: u64) -> StreamEvent {
    StreamEvent::Usage(Usage::reported(
        UsageCounts::new()
            .with(TokenKind::Input, input)
            .with(TokenKind::Output, output),
    ))
}

/// The completion-side usage `message_delta` reports, which carries no input count.
fn completed(output: u64) -> StreamEvent {
    StreamEvent::Usage(Usage::reported(
        UsageCounts::new().with(TokenKind::Output, output),
    ))
}

#[test]
fn a_text_stream_decodes_to_the_canonical_sequence() {
    let events = events(&[
        start(),
        text_block(0),
        text(0, "Hel"),
        text(0, "lo"),
        block_stop(0),
        finish("end_turn"),
        stop(),
    ]);
    assert_eq!(
        events,
        vec![
            start_event(),
            reported(10, 1),
            StreamEvent::TextDelta("Hel".to_owned()),
            StreamEvent::TextDelta("lo".to_owned()),
            completed(5),
            StreamEvent::Finish(FinishReason::Stop),
        ]
    );
}

#[test]
fn the_payload_type_is_what_the_decoder_reads() {
    // The framing repeats the payload's `type` in an `event:` line; a frame that carries only the
    // payload must decode the same way, and a disagreeing name must not change the meaning.
    let payload = json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": {"type": "text_delta", "text": "hi"},
    })
    .to_string();
    for frame in [
        event("content_block_delta", &payload),
        event("something_else", &payload),
        format!("data: {payload}\n\n"),
    ] {
        assert_eq!(
            events(&[
                start(),
                text_block(0),
                frame,
                block_stop(0),
                finish("end_turn"),
                stop()
            ])
            .into_iter()
            .filter(|event| matches!(event, StreamEvent::TextDelta(_)))
            .collect::<Vec<_>>(),
            vec![StreamEvent::TextDelta("hi".to_owned())],
            "the payload decides"
        );
    }
}

#[test]
fn usage_may_arrive_in_a_later_message_delta() {
    let events = events(&[
        start(),
        text_block(0),
        text(0, "hi"),
        block_stop(0),
        usage_only(),
        finish("max_tokens"),
        stop(),
    ]);
    assert_eq!(
        events,
        vec![
            start_event(),
            reported(10, 1),
            StreamEvent::TextDelta("hi".to_owned()),
            completed(4),
            completed(5),
            StreamEvent::Finish(FinishReason::Length),
        ]
    );
}

#[test]
fn any_fragmentation_produces_the_same_sequence() {
    let chunks = vec![
        start(),
        text_block(0),
        text(0, "a"),
        text(0, "b"),
        block_stop(0),
        finish("end_turn"),
        stop(),
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
fn a_reasoning_stream_carries_text_and_ignores_the_signature() {
    // `signature_delta` closes a thinking block with the provider's signature; the canonical stream
    // has no signature event and this adapter never sends a thinking block back, so the value has
    // nowhere to go. The text is what a client renders.
    let started = json_event(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "thinking", "thinking": ""},
        }),
    );
    let thinking = json_event(
        "content_block_delta",
        json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "thinking_delta", "thinking": "consider"},
        }),
    );
    let signature = json_event(
        "content_block_delta",
        json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "signature_delta", "signature": "sig-1"},
        }),
    );
    assert_eq!(
        events(&[
            start(),
            started,
            thinking,
            signature,
            block_stop(0),
            finish("end_turn"),
            stop(),
        ]),
        vec![
            start_event(),
            reported(10, 1),
            StreamEvent::ReasoningDelta("consider".to_owned()),
            completed(5),
            StreamEvent::Finish(FinishReason::Stop),
        ]
    );
}

#[test]
fn a_streamed_thinking_block_without_text_or_signature_is_refused() {
    let signature = json_event(
        "content_block_delta",
        json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "signature_delta"},
        }),
    );
    let started = json_event(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "thinking"},
        }),
    );
    assert!(matches!(
        push_err(&[start(), started, signature]),
        ProviderError::Malformed { .. }
    ));
}
