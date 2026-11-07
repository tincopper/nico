//! The framing: heartbeats, unknown events, broken frames and the failure frame.

use nico_core::ErrorCategory;
use nico_llm::{FinishReason, Provider, ProviderError, StreamEvent};
use serde_json::json;

use super::{
    block_stop, completed, finish, json_event, reported, start, start_event, stop, text, text_block,
};
use crate::support::{event, events, finish_err, provider, push_err};

#[test]
fn an_error_frame_becomes_a_stream_error_and_ends_the_decoder() {
    let failed = json_event(
        "error",
        json!({
            "type": "error",
            "error": {"type": "overloaded_error", "message": "secret upstream detail"},
        }),
    );
    let events = events(&[start(), failed, text_block(0), stop()]);
    assert_eq!(events.len(), 3, "nothing follows the error: {events:?}");
    assert_eq!(events[0], start_event());
    assert_eq!(events[1], reported(10, 1));
    let StreamEvent::Error(error) = &events[2] else {
        panic!("the last event must be the failure");
    };
    error.validate().expect("a mapped stream error is valid");
    assert_eq!(error.category(), ErrorCategory::Upstream);
    assert_eq!(error.code(), "upstream_error");
    assert_eq!(error.http_status(), 502);
    assert!(!error.retryable(), "a byte already reached the caller");
    assert_eq!(error.provider_code(), Some("overloaded_error"));
    assert!(
        !error.message().contains("secret upstream detail"),
        "the provider's message must not reach the caller"
    );
}

#[test]
fn a_broken_frame_is_refused_without_quoting_it() {
    let broken = event(
        "content_block_delta",
        "{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"MARKER\"}",
    );
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
    let error = finish_err(&[start(), "data: {\"type\":\"ping\"}".to_owned()]);
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
}

#[test]
fn heartbeats_and_empty_frames_are_ignored() {
    let events = events(&[
        ": keep-alive\n\n".to_owned(),
        start(),
        "data: \n\n".to_owned(),
        text_block(0),
        text(0, "hi"),
        "\n".to_owned(),
        block_stop(0),
        finish("end_turn"),
        stop(),
    ]);
    assert_eq!(
        events,
        vec![
            start_event(),
            reported(10, 1),
            StreamEvent::TextDelta("hi".to_owned()),
            completed(5),
            StreamEvent::Finish(FinishReason::Stop),
        ]
    );
}

#[test]
fn ping_and_unknown_event_types_are_skipped() {
    let ping = json_event("ping", json!({"type": "ping"}));
    let unknown = json_event(
        "something_new",
        json!({"type": "something_new", "value": 1}),
    );
    assert_eq!(
        events(&[
            start(),
            ping.clone(),
            unknown,
            text_block(0),
            text(0, "hi"),
            ping,
            block_stop(0),
            finish("end_turn"),
            stop(),
        ]),
        vec![
            start_event(),
            reported(10, 1),
            StreamEvent::TextDelta("hi".to_owned()),
            completed(5),
            StreamEvent::Finish(FinishReason::Stop),
        ],
        "a heartbeat is not content"
    );
}

#[test]
fn an_unknown_event_field_is_ignored() {
    // A stream fragment is not a document a client reads; an unmodelled top-level member has no
    // canonical event to ride on and is not a fact the answer changes.
    let annotated = json_event(
        "content_block_delta",
        json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "hi"},
            "ttft_ms": 12,
        }),
    );
    assert_eq!(
        events(&[
            start(),
            text_block(0),
            annotated,
            block_stop(0),
            finish("end_turn"),
            stop(),
        ]),
        vec![
            start_event(),
            reported(10, 1),
            StreamEvent::TextDelta("hi".to_owned()),
            completed(5),
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
        .push(b"data: {\"type\":")
        .expect("a partial frame is buffered, not refused");
    let events = events(&[
        start(),
        text_block(0),
        text(0, "hi"),
        block_stop(0),
        finish("end_turn"),
    ]);
    assert_eq!(events[2], StreamEvent::TextDelta("hi".to_owned()));
}
