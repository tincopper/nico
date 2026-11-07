//! Completeness and order: what ends a stream, and what a truncated one is not.

use nico_llm::{FinishReason, ProviderError, StreamEvent};
use serde_json::json;

use super::{
    block_stop, completed, finish, json_event, reported, start, start_event, stop, text, text_block,
};
use crate::support::{events, finish_err, push_err};

#[test]
fn a_body_that_ends_after_the_reason_is_complete_without_the_terminator() {
    let events = events(&[
        start(),
        text_block(0),
        text(0, "hi"),
        block_stop(0),
        finish("end_turn"),
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
fn a_body_that_ends_before_the_reason_is_refused() {
    let error = finish_err(&[start(), text_block(0), text(0, "hi"), block_stop(0), stop()]);
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "the client must not be told the model finished when nobody said it did: {error:?}"
    );
}

#[test]
fn a_stream_that_never_announces_its_response_is_refused() {
    let error = push_err(&[text_block(0)]);
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
}

#[test]
fn a_second_message_start_is_refused() {
    let error = push_err(&[start(), start()]);
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
}

#[test]
fn a_message_start_that_is_not_an_assistant_message_is_refused() {
    let wrong = json_event(
        "message_start",
        json!({
            "type": "message_start",
            "message": {"id": "msg_1", "model": "m", "role": "user"},
        }),
    );
    assert!(matches!(
        push_err(&[wrong]),
        ProviderError::Malformed { .. }
    ));
}

#[test]
fn content_after_the_finish_is_refused() {
    let error = push_err(&[
        start(),
        text_block(0),
        block_stop(0),
        finish("end_turn"),
        text_block(1),
    ]);
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
}

#[test]
fn a_finish_reason_reported_twice_is_refused() {
    let error = push_err(&[
        start(),
        text_block(0),
        block_stop(0),
        finish("end_turn"),
        finish("end_turn"),
    ]);
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
}

#[test]
fn a_finish_with_an_open_block_is_refused() {
    let error = push_err(&[start(), text_block(0), finish("end_turn")]);
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "a block without its end event would leave a client waiting: {error:?}"
    );
}
