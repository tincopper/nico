//! Encoding a canonical event stream as `GenerateContentResponse` chunks.

use nico_core::ErrorCategory;
use nico_llm::{
    CanonicalError, ClientStreamEncoder, FinishReason, ResponseId, StreamEvent, ToolCallId,
    ToolName, protocol::ClientCodec,
};
use serde_json::Value;

use crate::support::{codec, model};

/// The canonical start event every stream begins with.
fn start() -> StreamEvent {
    StreamEvent::Start {
        id: ResponseId::try_new("resp-1").expect("a valid id"),
        model: model(),
    }
}

/// A tool-call argument delta for one call.
fn delta(index: u32, arguments: &str) -> StreamEvent {
    StreamEvent::ToolCallDelta {
        id: ToolCallId::try_new("call_0").expect("a valid id"),
        arguments: arguments.to_owned(),
        index,
    }
}

/// Encodes one event and parses the chunk it produced, if any.
fn push(encoder: &mut Box<dyn ClientStreamEncoder>, event: &StreamEvent) -> Option<Value> {
    let bytes = encoder.push(event).expect("the event encodes");
    if bytes.is_empty() {
        return None;
    }
    let text = std::str::from_utf8(&bytes)
        .expect("an event frame is UTF-8")
        .strip_prefix("data: ")
        .expect("the shared SSE framing")
        .trim_end()
        .to_owned();
    Some(serde_json::from_str(&text).expect("a JSON chunk"))
}

/// A fresh encoder with the start event already written.
fn started() -> Box<dyn ClientStreamEncoder> {
    let mut encoder = codec().stream_encoder();
    push(&mut encoder, &start());
    encoder
}

/// The start event is the identity chunk; content arrives one chunk per event.
#[test]
fn start_opens_the_identity_and_content_is_one_chunk_per_event() {
    let mut encoder = started();

    let chunk = push(&mut encoder, &StreamEvent::TextDelta("hel".into())).expect("a chunk");
    assert!(
        chunk.get("responseId").is_none(),
        "the identity travels on the first chunk only"
    );
    assert_eq!(chunk["candidates"][0]["content"]["role"], "model");
    assert_eq!(chunk["candidates"][0]["content"]["parts"][0]["text"], "hel");

    let chunk = push(&mut encoder, &StreamEvent::ReasoningDelta("hmm".into())).expect("a chunk");
    let part = &chunk["candidates"][0]["content"]["parts"][0];
    assert_eq!(part["text"], "hmm");
    assert_eq!(part["thought"], true);

    assert!(
        push(&mut encoder, &StreamEvent::TextDelta(String::new())).is_none(),
        "a zero-length increment is no chunk at all"
    );
}

/// A tool call's arguments accumulate; the dialect's one `functionCall` part is written at the end.
#[test]
fn a_tool_call_is_written_once_its_arguments_close() {
    let mut encoder = started();
    push(
        &mut encoder,
        &StreamEvent::ToolCallStart {
            id: ToolCallId::try_new("call_0").expect("a valid id"),
            name: ToolName::try_new("lookup").expect("a valid name"),
            index: 0,
        },
    );
    assert!(
        push(&mut encoder, &delta(0, "{\"q\":")).is_none(),
        "the dialect cannot carry a fragment, so the call waits"
    );
    push(&mut encoder, &delta(0, "\"gemini\"}"));
    let chunk = push(
        &mut encoder,
        &StreamEvent::ToolCallEnd {
            id: ToolCallId::try_new("call_0").expect("a valid id"),
            index: 0,
        },
    )
    .expect("the closing chunk");
    let part = &chunk["candidates"][0]["content"]["parts"][0];
    assert_eq!(part["functionCall"]["id"], "call_0");
    assert_eq!(part["functionCall"]["name"], "lookup");
    assert_eq!(part["functionCall"]["args"]["q"], "gemini");
}

/// Usage is its own chunk, so a report that lands after the finish event still reaches the caller.
#[test]
fn usage_is_its_own_chunk_even_after_the_finish() {
    let mut encoder = started();
    let usage = StreamEvent::Usage(nico_llm::Usage::reported(
        nico_llm::UsageCounts::new().with(nico_llm::TokenKind::Input, 11),
    ));
    let chunk = push(&mut encoder, &usage).expect("a usage chunk");
    assert_eq!(chunk["usageMetadata"]["promptTokenCount"], 11);

    push(&mut encoder, &StreamEvent::Finish(FinishReason::Stop));
    let chunk = push(&mut encoder, &usage).expect("the late report is still written");
    assert_eq!(chunk["usageMetadata"]["promptTokenCount"], 11);
}

/// The finish event is the reason chunk; content after it is ignored, not refused.
#[test]
fn the_finish_chunk_says_why_and_later_content_is_ignored() {
    let mut encoder = started();
    let chunk =
        push(&mut encoder, &StreamEvent::Finish(FinishReason::ToolCalls)).expect("a finish chunk");
    assert_eq!(chunk["candidates"][0]["finishReason"], "STOP");
    assert!(chunk["candidates"][0].get("content").is_none());

    assert!(
        push(&mut encoder, &StreamEvent::TextDelta("late".into())).is_none(),
        "the answer is over"
    );
    assert!(
        push(&mut encoder, &StreamEvent::Finish(FinishReason::Stop)).is_none(),
        "one reason is all the answer carries"
    );
    assert!(encoder.finish().expect("a clean end").is_empty());
}

/// Content before the start event is a gateway defect, not a client answer.
#[test]
fn an_event_before_the_start_is_an_encode_failure() {
    let mut encoder = codec().stream_encoder();
    for event in [
        StreamEvent::TextDelta("x".into()),
        StreamEvent::ReasoningDelta("y".into()),
        StreamEvent::Finish(FinishReason::Stop),
    ] {
        let error = encoder
            .push(&event)
            .expect_err("the stream has no identity yet");
        assert!(
            matches!(error, nico_llm::CodecError::Encode { .. }),
            "got {error:?}"
        );
    }
}

/// A stream that ends with an open tool call is the defect `finish()` exists to catch.
#[test]
fn a_call_whose_arguments_never_closed_is_a_defect() {
    let mut encoder = started();
    push(
        &mut encoder,
        &StreamEvent::ToolCallStart {
            id: ToolCallId::try_new("call_0").expect("a valid id"),
            name: ToolName::try_new("lookup").expect("a valid name"),
            index: 0,
        },
    );
    push(&mut encoder, &delta(0, "{\"q\":"));
    let error = encoder
        .finish()
        .expect_err("the canonical stream never leaves a call open");
    assert!(
        matches!(error, nico_llm::CodecError::Encode { .. }),
        "got {error:?}"
    );
}

/// A fragment for a call that has not started is a defect too.
#[test]
fn a_fragment_before_its_call_started_is_a_defect() {
    let mut encoder = started();
    let error = encoder
        .push(&delta(3, "{}"))
        .expect_err("no call of index 3 is open");
    assert!(
        matches!(error, nico_llm::CodecError::Encode { .. }),
        "got {error:?}"
    );
}

/// A failure after the start is one `data:` frame of the error envelope, then silence.
#[test]
fn a_failing_stream_writes_the_error_frame_and_stops() {
    let mut encoder = started();
    let failure = CanonicalError::try_new(
        ErrorCategory::Upstream,
        "upstream_error",
        "the upstream failed to serve the request",
        502,
    )
    .expect("a valid error");
    let bytes = encoder
        .push(&StreamEvent::Error(failure))
        .expect("the failure frames");
    let text = std::str::from_utf8(&bytes).expect("UTF-8");
    assert!(
        text.starts_with("data: {\"error\":{\"code\":502,"),
        "got {text}"
    );

    let document: Value =
        serde_json::from_str(text.strip_prefix("data: ").expect("the framing").trim_end())
            .expect("a JSON envelope");
    assert_eq!(document["error"]["status"], "UNAVAILABLE");
    assert!(
        encoder
            .finish()
            .expect("nothing after a failure")
            .is_empty(),
        "the stream already ended"
    );
}

/// The arguments must concatenate into a JSON object; anything else is a defect.
#[test]
fn arguments_that_do_not_concatenate_are_a_defect() {
    let mut encoder = started();
    push(
        &mut encoder,
        &StreamEvent::ToolCallStart {
            id: ToolCallId::try_new("call_0").expect("a valid id"),
            name: ToolName::try_new("lookup").expect("a valid name"),
            index: 0,
        },
    );
    push(&mut encoder, &delta(0, "not json"));
    let error = encoder
        .push(&StreamEvent::ToolCallEnd {
            id: ToolCallId::try_new("call_0").expect("a valid id"),
            index: 0,
        })
        .expect_err("the fragments never formed an object");
    assert!(
        matches!(error, nico_llm::CodecError::Encode { .. }),
        "got {error:?}"
    );
}

/// A second start event is a defect: one stream is one answer.
#[test]
fn a_second_start_is_a_defect() {
    let mut encoder = started();
    let error = encoder
        .push(&start())
        .expect_err("the stream already has an identity");
    assert!(
        matches!(error, nico_llm::CodecError::Encode { .. }),
        "got {error:?}"
    );
}

/// The model on the identity chunk is the physical one the canonical stream named.
#[test]
fn the_identity_chunk_names_the_physical_model() {
    let mut encoder = codec().stream_encoder();
    let chunk = push(&mut encoder, &start()).expect("the identity chunk");
    assert_eq!(chunk["responseId"], "resp-1");
    assert_eq!(chunk["modelVersion"], model().as_str());
    assert!(
        chunk.get("candidates").is_none(),
        "the first chunk need not carry content"
    );
}

/// The raw-arguments value is checked, not trusted: a JSON scalar is refused.
#[test]
fn arguments_that_concatenate_to_a_scalar_are_a_defect() {
    let mut encoder = started();
    push(
        &mut encoder,
        &StreamEvent::ToolCallStart {
            id: ToolCallId::try_new("call_0").expect("a valid id"),
            name: ToolName::try_new("lookup").expect("a valid name"),
            index: 0,
        },
    );
    push(&mut encoder, &delta(0, "42"));
    let error = encoder
        .push(&StreamEvent::ToolCallEnd {
            id: ToolCallId::try_new("call_0").expect("a valid id"),
            index: 0,
        })
        .expect_err("arguments are an object");
    assert!(
        matches!(error, nico_llm::CodecError::Encode { .. }),
        "got {error:?}"
    );
}
