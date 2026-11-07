//! The streamed path: SSE framing, event order, and the completeness rule.

use nico_llm::{FinishReason, Provider, ProviderError, StreamEvent};
use serde_json::json;

use crate::support::{
    MODEL_VERSION, RESPONSE_ID, events, events_at, finish_err, frame, push_err, stream,
};

/// The answer most stream tests read.
fn answer() -> Vec<String> {
    vec![
        stream(&[json!({
            "candidates": [{"content": {"role": "model", "parts": [{"text": "Hel"}]}}],
            "modelVersion": MODEL_VERSION,
            "responseId": RESPONSE_ID
        })]),
        stream(&[json!({
            "candidates": [{"content": {"parts": [{"text": "lo"}]}, "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 2, "candidatesTokenCount": 1}
        })]),
    ]
}

/// Returns the canonical shape of the events, without the values that identify the answer.
fn summary(events: &[StreamEvent]) -> Vec<&'static str> {
    events
        .iter()
        .map(|event| match event {
            StreamEvent::Start { .. } => "start",
            StreamEvent::TextDelta(_) => "text",
            StreamEvent::ReasoningDelta(_) => "reasoning",
            StreamEvent::ToolCallStart { .. } => "call-start",
            StreamEvent::ToolCallDelta { .. } => "call-delta",
            StreamEvent::ToolCallEnd { .. } => "call-end",
            StreamEvent::Usage(_) => "usage",
            StreamEvent::Finish(_) => "finish",
            StreamEvent::Error(_) => "error",
            _ => "other",
        })
        .collect()
}

#[test]
fn the_events_do_not_depend_on_where_the_chunks_split() {
    let chunks = answer();
    let whole = events(&chunks);
    let total = chunks.concat().len();
    for size in 1..=total {
        assert_eq!(
            events_at(&chunks, size),
            whole,
            "a frame split at {size} bytes must decode to the same events"
        );
    }
}

#[test]
fn several_frames_in_one_chunk_are_all_read() {
    let chunks = answer();
    let joined = chunks.concat();
    assert_eq!(events(&[joined]), events(&chunks));
}

#[test]
fn the_event_order_is_start_text_usage_finish() {
    assert_eq!(
        summary(&events(&answer())),
        vec!["start", "text", "text", "usage", "finish"],
        "the accounting of the chunk that carries the finish reason precedes its end"
    );
}

#[test]
fn a_heartbeat_or_a_done_sentinel_adds_nothing() {
    let chunks = vec![
        ": keep-alive\n\n".to_owned(),
        frame(&json!({
            "candidates": [{"content": {"parts": [{"text": "hi"}]}, "finishReason": "STOP"}],
            "modelVersion": MODEL_VERSION
        })),
        "data: [DONE]\n\n".to_owned(),
        "\n\n".to_owned(),
    ];
    let events = events(&chunks);
    assert_eq!(summary(&events), vec!["start", "text", "finish"]);
}

#[test]
fn a_body_that_ends_before_a_finish_reason_is_refused() {
    let error = finish_err(&[stream(&[json!({
        "candidates": [{"content": {"parts": [{"text": "hi"}]}}],
        "modelVersion": MODEL_VERSION
    })])]);
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "the caller must not be told the model finished when nobody said it did: {error:?}"
    );
}

#[test]
fn bytes_after_the_finish_reason_are_ignored() {
    let chunks = vec![
        stream(&[json!({
            "candidates": [{"content": {"parts": [{"text": "done"}]}, "finishReason": "STOP"}],
            "modelVersion": MODEL_VERSION
        })]),
        stream(&[json!({"candidates": [{"content": {"parts": [{"text": "second answer"}]}}]})]),
    ];
    let events = events(&chunks);
    assert_eq!(summary(&events), vec!["start", "text", "finish"]);
}

#[test]
fn a_stream_that_changes_the_model_it_names_is_refused() {
    let chunks = vec![stream(&[
        json!({
            "candidates": [{"content": {"parts": [{"text": "one"}]}}],
            "modelVersion": MODEL_VERSION
        }),
        json!({
            "candidates": [{"content": {"parts": [{"text": "two"}]}}],
            "modelVersion": "gemini-2.0-pro"
        }),
    ])];
    let error = push_err(&chunks);
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
}

#[test]
fn a_stream_that_changes_the_response_it_names_is_refused() {
    let chunks = vec![stream(&[
        json!({
            "candidates": [{"content": {"parts": [{"text": "one"}]}}],
            "modelVersion": MODEL_VERSION,
            "responseId": RESPONSE_ID
        }),
        json!({"candidates": [], "responseId": "resp-2"}),
    ])];
    let error = push_err(&chunks);
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
}

#[test]
fn a_repeated_identity_is_accepted() {
    let chunks = vec![stream(&[
        json!({
            "candidates": [{"content": {"parts": [{"text": "one"}]}}],
            "modelVersion": MODEL_VERSION,
            "responseId": RESPONSE_ID
        }),
        json!({
            "candidates": [{"content": {"parts": [{"text": "two"}]}, "finishReason": "STOP"}],
            "modelVersion": MODEL_VERSION,
            "responseId": RESPONSE_ID
        }),
    ])];
    assert_eq!(
        summary(&events(&chunks)),
        vec!["start", "text", "text", "finish"]
    );
}

#[test]
fn a_frame_that_is_not_a_document_is_refused() {
    let error = push_err(&["data: {not json\n\n".to_owned()]);
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
}

#[test]
fn a_complete_stream_reports_nothing_more_at_its_end() {
    let mut decoder = crate::support::provider().stream_decoder();
    for chunk in answer() {
        decoder
            .push(chunk.as_bytes())
            .expect("the frames are readable");
    }
    // The last frame completed the answer, so the end of the body adds no event and no failure.
    assert!(decoder.finish().expect("the answer is complete").is_empty());
}

#[test]
fn a_stream_with_a_reasoning_delta_and_a_call_carries_both() {
    let chunks = vec![stream(&[
        json!({
            "candidates": [{"content": {"role": "model", "parts": [
                {"text": "consider", "thought": true},
                {"functionCall": {"name": "weather", "args": {"city": "Oslo"}}}
            ]}}],
            "modelVersion": MODEL_VERSION
        }),
        json!({"candidates": [{"finishReason": "STOP"}]}),
    ])];
    assert_eq!(
        summary(&events(&chunks)),
        vec![
            "start",
            "reasoning",
            "call-start",
            "call-delta",
            "call-end",
            "finish"
        ]
    );
}

#[test]
fn a_finish_reason_of_a_content_filter_carries_no_content() {
    let chunks = vec![stream(&[json!({
        "candidates": [{"finishReason": "SAFETY"}],
        "modelVersion": MODEL_VERSION
    })])];
    let events = events(&chunks);
    assert_eq!(summary(&events), vec!["start", "finish"]);
    assert!(matches!(
        events.last(),
        Some(StreamEvent::Finish(FinishReason::ContentFilter))
    ));
}
