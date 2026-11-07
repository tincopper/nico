//! Encoding canonical stream events as named Messages events.

use nico_llm::{
    ClientCodec, ClientStreamEncoder,
    protocol::CodecError,
    schema::{
        CanonicalError, FinishReason, StreamEvent, TokenKind, ToolCallId, ToolName, Usage,
        UsageCounts,
    },
};

use super::support::{call, usage};

/// The encoder under test.
fn encoder() -> Box<dyn ClientStreamEncoder> {
    super::support::codec().stream_encoder()
}

/// Collects every frame of a sequence of events.
fn run(events: &[StreamEvent]) -> String {
    let mut encoder = encoder();
    let mut bytes = Vec::new();
    for event in events {
        bytes.extend(encoder.push(event).expect("the event is encodable"));
    }
    bytes.extend(encoder.finish().expect("the terminator is encodable"));
    String::from_utf8(bytes).expect("the frames are UTF-8")
}

/// The event names of one stream, in order.
fn names(frames: &str) -> Vec<&str> {
    frames
        .lines()
        .filter_map(|line| line.strip_prefix("event: "))
        .collect()
}

#[test]
fn a_text_answer_is_a_named_event_sequence() {
    let start = StreamEvent::Start {
        id: nico_llm::ResponseId::try_new("msg_01").expect("a valid id"),
        model: super::support::model(),
    };
    let frames = run(&[
        start,
        StreamEvent::TextDelta("hel".to_owned()),
        StreamEvent::TextDelta("lo".to_owned()),
        StreamEvent::Finish(FinishReason::Stop),
    ]);

    assert_eq!(
        names(&frames),
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ]
    );
    assert!(
        frames.contains(r#""type":"text_delta","text":"hel""#),
        "{frames}"
    );
    assert!(frames.contains(r#""stop_reason":"end_turn""#), "{frames}");
}

#[test]
fn a_tool_call_opens_its_own_block_and_closes_the_text_block_first() {
    let start = StreamEvent::Start {
        id: nico_llm::ResponseId::try_new("msg_01").expect("a valid id"),
        model: super::support::model(),
    };
    let call = call("toolu_1", "lookup", "{}");
    let tools = [
        StreamEvent::ToolCallStart {
            id: ToolCallId::try_new("toolu_1").expect("a valid id"),
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
    ];
    let mut events = vec![start, StreamEvent::TextDelta("calling".to_owned())];
    events.extend(tools.iter().cloned());
    events.push(StreamEvent::Finish(FinishReason::ToolCalls));
    let frames = run(&events);

    assert_eq!(
        names(&frames),
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ]
    );
    assert!(
        frames.contains(r#""type":"tool_use","id":"toolu_1""#),
        "{frames}"
    );
    assert!(frames.contains(r#""partial_json":"{\"q\""#), "{frames}");
    assert!(frames.contains(r#""stop_reason":"tool_use""#), "{frames}");
}

#[test]
fn a_usage_report_that_arrives_late_still_lands_before_the_terminator() {
    let start = StreamEvent::Start {
        id: nico_llm::ResponseId::try_new("msg_01").expect("a valid id"),
        model: super::support::model(),
    };
    let frames = run(&[
        start,
        StreamEvent::TextDelta("x".to_owned()),
        StreamEvent::Finish(FinishReason::Stop),
        StreamEvent::Usage(usage()),
    ]);
    assert_eq!(
        names(&frames),
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_delta",
            "message_stop",
        ],
        "the second delta carries the late report; the terminator is last"
    );
    assert!(
        frames.contains(r#""cache_read_input_tokens":3"#),
        "{frames}"
    );
}

#[test]
fn usage_that_arrives_before_the_finish_event_rides_the_first_delta() {
    let start = StreamEvent::Start {
        id: nico_llm::ResponseId::try_new("msg_01").expect("a valid id"),
        model: super::support::model(),
    };
    let frames = run(&[
        start,
        StreamEvent::Usage(Usage::reported(
            UsageCounts::new().with(TokenKind::Output, 2),
        )),
        StreamEvent::Finish(FinishReason::Stop),
    ]);
    assert_eq!(
        names(&frames),
        ["message_start", "message_delta", "message_stop",]
    );
    assert!(frames.contains(r#""output_tokens":2"#), "{frames}");
}

#[test]
fn a_fragment_before_its_block_is_an_encoder_defect() {
    let mut encoder = encoder();
    let start = StreamEvent::Start {
        id: nico_llm::ResponseId::try_new("msg_01").expect("a valid id"),
        model: super::support::model(),
    };
    encoder.push(&start).expect("the start event encodes");
    let error = encoder
        .push(&StreamEvent::ToolCallDelta {
            id: ToolCallId::try_new("toolu_1").expect("a valid id"),
            arguments: "{}".to_owned(),
            index: 7,
        })
        .expect_err("no block was opened for that call");
    assert!(matches!(error, CodecError::Encode { .. }));
}

#[test]
fn a_failing_stream_ends_with_the_error_event_and_no_terminator() {
    let start = StreamEvent::Start {
        id: nico_llm::ResponseId::try_new("msg_01").expect("a valid id"),
        model: super::support::model(),
    };
    let failure = CanonicalError::try_new(
        nico_core::ErrorCategory::Upstream,
        "upstream_error",
        "the provider failed",
        502,
    )
    .expect("a valid error");
    let frames = run(&[
        start,
        StreamEvent::TextDelta("x".to_owned()),
        StreamEvent::Error(failure),
    ]);
    assert_eq!(
        names(&frames),
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "error"
        ]
    );
}
