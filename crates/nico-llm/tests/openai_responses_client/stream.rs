//! Encoding canonical stream events as Responses events.

use nico_llm::{
    ClientCodec, ClientStreamEncoder,
    protocol::CodecError,
    schema::{
        CanonicalError, FinishReason, StreamEvent, TokenKind, ToolCallId, ToolName, Usage,
        UsageCounts,
    },
};

use super::support::{call, usage};

/// Parses the frames of one stream into `(event name, payload)` pairs.
fn run(events: &[StreamEvent]) -> Vec<(String, serde_json::Value)> {
    let mut encoder: Box<dyn ClientStreamEncoder> = super::support::codec().stream_encoder();
    let mut bytes = Vec::new();
    for event in events {
        bytes.extend(encoder.push(event).expect("the event is encodable"));
    }
    bytes.extend(encoder.finish().expect("the terminator is encodable"));
    let text = String::from_utf8(bytes).expect("the frames are UTF-8");
    text.split("\n\n")
        .filter(|frame| !frame.is_empty())
        .map(|frame| {
            let mut name = String::new();
            let mut data = String::new();
            for line in frame.lines() {
                if let Some(rest) = line.strip_prefix("event: ") {
                    name = rest.to_owned();
                } else if let Some(rest) = line.strip_prefix("data: ") {
                    data = rest.to_owned();
                }
            }
            let payload: serde_json::Value =
                serde_json::from_str(&data).expect("the payload is JSON");
            if name.is_empty() {
                // The failure envelope is the one both OpenAI protocols share, and it is framed as a
                // bare `data:` line, so it has no event name of its own.
                assert!(payload.get("error").is_some(), "{payload}");
            } else {
                assert_eq!(
                    name, payload["type"],
                    "the event name repeats the payload's type"
                );
            }
            (name, payload)
        })
        .collect()
}

/// The start event every stream opens with.
fn start() -> StreamEvent {
    StreamEvent::Start {
        id: nico_llm::ResponseId::try_new("resp_01").expect("a valid id"),
        model: super::support::model(),
    }
}

#[test]
fn a_text_answer_is_an_event_sequence_that_ends_with_the_completed_response() {
    let frames = run(&[
        start(),
        StreamEvent::TextDelta("hel".to_owned()),
        StreamEvent::TextDelta("lo".to_owned()),
        StreamEvent::Finish(FinishReason::Stop),
    ]);
    let names: Vec<&str> = frames.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        [
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.delta",
            "response.output_text.done",
            "response.output_item.done",
            "response.completed",
        ]
    );
    let completed = &frames.last().expect("a last frame").1;
    assert_eq!(completed["response"]["status"], "completed");
    assert_eq!(
        completed["response"]["output"][0]["content"][0]["text"],
        "hello"
    );
}

#[test]
fn the_sequence_number_starts_at_zero_and_never_repeats() {
    let frames = run(&[
        start(),
        StreamEvent::TextDelta("x".to_owned()),
        StreamEvent::Finish(FinishReason::Stop),
    ]);
    for (position, (_, payload)) in frames.iter().enumerate() {
        assert_eq!(payload["sequence_number"], position, "{payload}");
    }
}

#[test]
fn a_call_streams_its_arguments_and_reaches_the_completion_event() {
    let call = call("call_1", "lookup", "{}");
    let frames = run(&[
        start(),
        StreamEvent::ToolCallStart {
            id: ToolCallId::try_new("call_1").expect("a valid id"),
            name: ToolName::try_new("lookup").expect("a valid name"),
            index: 0,
        },
        StreamEvent::ToolCallDelta {
            id: call.id().clone(),
            arguments: r#"{"q""#.to_owned(),
            index: 0,
        },
        StreamEvent::ToolCallDelta {
            id: call.id().clone(),
            arguments: r#":"x"}"#.to_owned(),
            index: 0,
        },
        StreamEvent::ToolCallEnd {
            id: call.id().clone(),
            index: 0,
        },
        StreamEvent::Finish(FinishReason::ToolCalls),
    ]);
    let names: Vec<&str> = frames.iter().map(|(name, _)| name.as_str()).collect();
    assert!(
        names.contains(&"response.function_call_arguments.delta"),
        "{names:?}"
    );
    let completed = &frames.last().expect("a last frame").1;
    assert_eq!(completed["response"]["output"][0]["type"], "function_call");
    assert_eq!(
        completed["response"]["output"][0]["arguments"],
        r#"{"q":"x"}"#
    );
}

#[test]
fn a_late_usage_report_lands_in_the_completion_event() {
    let frames = run(&[
        start(),
        StreamEvent::TextDelta("x".to_owned()),
        StreamEvent::Finish(FinishReason::Stop),
        StreamEvent::Usage(usage()),
    ]);
    let completed = &frames.last().expect("a last frame").1;
    assert_eq!(completed["response"]["usage"]["output_tokens"], 7);
}

#[test]
fn a_body_that_ends_without_a_finish_event_is_not_dressed_up_as_complete() {
    let mut encoder: Box<dyn ClientStreamEncoder> = super::support::codec().stream_encoder();
    encoder.push(&start()).expect("the start event encodes");
    encoder
        .push(&StreamEvent::TextDelta("x".to_owned()))
        .expect("the delta encodes");
    let tail = encoder.finish().expect("the terminator is encodable");
    assert!(
        tail.is_empty(),
        "a completed event would claim an answer nobody finished"
    );
}

#[test]
fn a_failing_stream_ends_with_the_error_event() {
    let failure = CanonicalError::try_new(
        nico_core::ErrorCategory::Upstream,
        "upstream_error",
        "the provider failed",
        502,
    )
    .expect("a valid error");
    let frames = run(&[
        start(),
        StreamEvent::Usage(Usage::reported(
            UsageCounts::new().with(TokenKind::Output, 1),
        )),
        StreamEvent::Error(failure),
    ]);
    let last = frames.last().expect("a last frame");
    assert!(
        last.0.is_empty(),
        "the shared failure frame carries no event name"
    );
    assert_eq!(last.1["error"]["type"], "server_error");
}

#[test]
fn a_fragment_before_its_item_is_an_encoder_defect() {
    let mut encoder: Box<dyn ClientStreamEncoder> = super::support::codec().stream_encoder();
    encoder.push(&start()).expect("the start event encodes");
    let error = encoder
        .push(&StreamEvent::ToolCallDelta {
            id: ToolCallId::try_new("call_1").expect("a valid id"),
            arguments: "{}".to_owned(),
            index: 4,
        })
        .expect_err("no item was opened for that call");
    assert!(matches!(error, CodecError::Encode { .. }));
}
