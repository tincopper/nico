//! Decoding a Responses event stream: event order, completeness and failures.
//!
//! The tool-call assembler lives in [`calls`].

use nico_llm::{
    FinishReason, Provider, StreamEvent, TokenKind,
    provider::openai_responses::OpenAiResponsesProvider,
};
use serde_json::{Value, json};

use crate::support::{events, events_at, finish_err, frame, json_event, push_err};

mod calls;

/// The identity every fixture announces.
const MODEL: &str = "gpt-small-2026-01-01";

/// The `response.created` frame.
fn created() -> String {
    json_event(json!({
        "type": "response.created",
        "sequence_number": 0,
        "response": {
            "id": "resp_1",
            "object": "response",
            "status": "in_progress",
            "model": MODEL,
            "output": [],
        },
    }))
}

/// The `response.completed` frame, with an optional usage document.
fn completed(usage: Value) -> String {
    let mut response = json!({
        "id": "resp_1",
        "object": "response",
        "status": "completed",
        "model": MODEL,
        "output": [],
    });
    if !usage.is_null() {
        response["usage"] = usage;
    }
    json_event(json!({
        "type": "response.completed",
        "sequence_number": 9,
        "response": response,
    }))
}

/// A usage document with the four canonical counts.
fn usage() -> Value {
    json!({
        "input_tokens": 10,
        "output_tokens": 5,
        "total_tokens": 15,
        "input_tokens_details": {"cached_tokens": 3},
        "output_tokens_details": {"reasoning_tokens": 2},
    })
}

/// The three events of a text answer.
fn text_stream() -> Vec<String> {
    vec![
        created(),
        json_event(json!({
            "type": "response.output_text.delta",
            "sequence_number": 1,
            "item_id": "msg_1",
            "output_index": 0,
            "content_index": 0,
            "delta": "hel",
        })),
        json_event(json!({
            "type": "response.output_text.delta",
            "sequence_number": 2,
            "item_id": "msg_1",
            "output_index": 0,
            "content_index": 0,
            "delta": "lo",
        })),
        completed(usage()),
    ]
}

#[test]
fn a_text_answer_is_a_start_deltas_usage_and_a_finish() {
    let events = events(&text_stream());
    assert_eq!(
        events,
        vec![
            StreamEvent::Start {
                id: nico_llm::ResponseId::try_new("resp_1").expect("a valid id"),
                model: nico_llm::ModelId::try_new(MODEL).expect("a valid model"),
            },
            StreamEvent::TextDelta("hel".to_owned()),
            StreamEvent::TextDelta("lo".to_owned()),
            StreamEvent::Usage(nico_llm::Usage::reported(
                nico_llm::UsageCounts::new()
                    .with(TokenKind::Input, 10)
                    .with(TokenKind::Output, 5)
                    .with(TokenKind::CacheRead, 3)
                    .with(TokenKind::Reasoning, 2)
            )),
            StreamEvent::Finish(FinishReason::Stop),
        ]
    );
}

#[test]
fn the_events_do_not_depend_on_the_chunk_boundaries() {
    let chunks = text_stream();
    let expected = events(&chunks);
    for size in 1..=8 {
        assert_eq!(
            events_at(&chunks, size),
            expected,
            "a stream read in {size}-byte chunks must produce the same events"
        );
    }
}

#[test]
fn a_reasoning_summary_is_rendered_as_reasoning() {
    let chunks = vec![
        created(),
        json_event(json!({
            "type": "response.reasoning_summary_text.delta",
            "output_index": 0,
            "item_id": "rs_1",
            "summary_index": 0,
            "delta": "thinking",
        })),
        completed(json!(null)),
    ];
    assert_eq!(
        events(&chunks)[1],
        StreamEvent::ReasoningDelta("thinking".to_owned()),
        "the summary is part of the answer the caller receives"
    );
}

#[test]
fn usage_without_counts_is_not_a_reported_zero() {
    // The event reports what the document said: an empty usage object is "nothing reported", which
    // is a different fact from a reported bucket of zeros.
    let chunks = vec![created(), completed(json!({}))];
    let events = events(&chunks);
    assert_eq!(
        events[1],
        StreamEvent::Usage(nico_llm::Usage::default()),
        "{events:?}"
    );
}

#[test]
fn a_stream_that_ends_before_its_completed_event_is_refused() {
    let chunks = vec![created(), frame("[DONE]")];
    assert!(
        matches!(
            finish_err(&chunks),
            nico_llm::ProviderError::Malformed { .. }
        ),
        "`[DONE]` is not this dialect's terminator"
    );
}

#[test]
fn every_byte_after_the_completed_event_is_ignored() {
    // The completed event ends the stream, so a second one (or a late fragment) can only be a
    // duplicated tail; it changes nothing the caller already received.
    let chunks = vec![
        created(),
        completed(json!(null)),
        json_event(json!({"type": "response.output_text.delta", "delta": "late"})),
        completed(json!(null)),
    ];
    assert_eq!(
        events(&chunks),
        vec![
            StreamEvent::Start {
                id: nico_llm::ResponseId::try_new("resp_1").expect("a valid id"),
                model: nico_llm::ModelId::try_new(MODEL).expect("a valid model"),
            },
            StreamEvent::Finish(FinishReason::Stop),
        ]
    );
}

#[test]
fn content_before_the_response_is_announced_is_refused() {
    let chunks = vec![json_event(json!({
        "type": "response.output_text.delta",
        "delta": "hi",
    }))];
    assert!(matches!(
        push_err(&chunks),
        nico_llm::ProviderError::Malformed { .. }
    ));
}

#[test]
fn announcing_the_response_twice_is_refused() {
    let chunks = vec![created(), created()];
    assert!(matches!(
        push_err(&chunks),
        nico_llm::ProviderError::Malformed { .. }
    ));
}

#[test]
fn an_unknown_output_item_type_is_refused() {
    let chunks = vec![
        created(),
        json_event(json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "web_search_call", "id": "ws_1"},
        })),
    ];
    assert!(matches!(
        push_err(&chunks),
        nico_llm::ProviderError::Malformed { .. }
    ));
}

#[test]
fn a_failed_answer_becomes_a_stream_error() {
    let chunks = vec![
        created(),
        json_event(json!({
            "type": "response.failed",
            "sequence_number": 2,
            "response": {
                "id": "resp_1",
                "object": "response",
                "status": "failed",
                "model": MODEL,
                "output": [],
                "error": {"code": "server_error", "message": "PROVIDER-MARKER"},
            },
        })),
    ];
    let events = events(&chunks);
    match &events[1] {
        StreamEvent::Error(error) => {
            assert_eq!(error.code(), "upstream_error");
            assert_eq!(error.provider_code(), Some("server_error"));
        }
        other => panic!("a failed answer must be a stream error: {other:?}"),
    }
    assert_eq!(events.len(), 2, "no finish event follows the failure");
    assert!(
        !format!("{events:?}").contains("PROVIDER-MARKER"),
        "the provider's message never reaches the caller"
    );
}

#[test]
fn a_bare_error_event_becomes_a_stream_error() {
    let chunks = vec![
        created(),
        json_event(json!({"type": "error", "code": "bad_request", "message": "PROVIDER-MARKER"})),
    ];
    let events = events(&chunks);
    match &events[1] {
        StreamEvent::Error(error) => assert_eq!(error.provider_code(), Some("bad_request")),
        other => panic!("an error event must be a stream error: {other:?}"),
    }
    assert!(!format!("{events:?}").contains("PROVIDER-MARKER"));
}

#[test]
fn the_framing_events_are_skipped() {
    let chunks = vec![
        created(),
        json_event(json!({"type": "response.in_progress", "response": {"id": "resp_1"}})),
        json_event(json!({
            "type": "response.content_part.added",
            "output_index": 0,
            "content_index": 0,
            "part": {"type": "output_text", "text": ""},
        })),
        json_event(json!({
            "type": "response.output_text.done",
            "output_index": 0,
            "content_index": 0,
            "text": "hello",
        })),
        json_event(json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {"type": "message"},
        })),
        frame("[DONE]"),
        completed(json!(null)),
    ];
    assert_eq!(
        events(&chunks),
        vec![
            StreamEvent::Start {
                id: nico_llm::ResponseId::try_new("resp_1").expect("a valid id"),
                model: nico_llm::ModelId::try_new(MODEL).expect("a valid model"),
            },
            StreamEvent::Finish(FinishReason::Stop),
        ],
        "framing adds no content, and `[DONE]` is not this dialect's terminator"
    );
}

#[test]
fn every_decoder_is_its_own_stream() {
    // A partial frame from one response must never be completed by another's bytes.
    let mut first = OpenAiResponsesProvider::new().stream_decoder();
    first
        .push(b"data: {\"type\":\"response.cre")
        .expect("a partial frame");
    let mut second = OpenAiResponsesProvider::new().stream_decoder();
    second
        .push(created().as_bytes())
        .expect("the second decoder starts at a frame boundary");
    assert!(matches!(
        first.finish(),
        Err(nico_llm::ProviderError::Malformed { .. })
    ));
}
