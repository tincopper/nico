//! What a streamed answer becomes: chunks that repeat the response's identity and the terminator.

use nico_core::ErrorCategory;
use nico_llm::{
    CanonicalError, ClientCodec, FinishReason, ModelId, ResponseId, StreamEvent, TokenKind,
    ToolCallId, ToolName, Usage, UsageCounts,
};

use super::support::{chunks, codec, encode_stream, frames};

/// The events of a text answer, up to the finish chunk.
fn events() -> Vec<StreamEvent> {
    vec![
        StreamEvent::Start {
            id: ResponseId::try_new("chatcmpl-9").expect("a valid id"),
            model: ModelId::try_new("chat-small-2026-01-01").expect("a valid model"),
        },
        StreamEvent::TextDelta("Two ".to_owned()),
        StreamEvent::TextDelta("cats.".to_owned()),
    ]
}

#[test]
fn every_chunk_repeats_the_response_identity_and_the_stream_terminates() {
    let mut events = events();
    events.push(StreamEvent::Finish(FinishReason::Stop));
    let stream = encode_stream(&events);
    let payloads = frames(&stream);
    assert_eq!(
        payloads.last().map(String::as_str),
        Some("[DONE]"),
        "a finished stream ends with the terminator"
    );

    let chunks = chunks(&stream);
    assert_eq!(chunks.len(), 4, "start, two deltas and the finish chunk");
    for chunk in &chunks {
        assert_eq!(chunk["id"], "chatcmpl-9");
        assert_eq!(chunk["model"], "chat-small-2026-01-01");
        assert_eq!(chunk["object"], "chat.completion.chunk");
        assert_eq!(chunk["created"], 0);
    }
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "Two ");
    assert_eq!(chunks[2]["choices"][0]["delta"]["content"], "cats.");
    assert_eq!(chunks[3]["choices"][0]["delta"], serde_json::json!({}));
    assert_eq!(chunks[3]["choices"][0]["finish_reason"], "stop");
}

#[test]
fn a_reasoning_delta_is_written_on_the_delta() {
    let mut events = events();
    events.insert(1, StreamEvent::ReasoningDelta("thinking".to_owned()));
    events.push(StreamEvent::Finish(FinishReason::Stop));
    let chunks = chunks(&encode_stream(&events));
    assert_eq!(
        chunks[1]["choices"][0]["delta"]["reasoning_content"],
        "thinking"
    );
    assert!(
        chunks[1]["choices"][0]["delta"].get("content").is_none(),
        "a reasoning chunk does not invent visible text"
    );
}

#[test]
fn an_empty_text_delta_is_still_a_chunk() {
    // Most providers send an empty delta for the turn's role, and a client appending it writes
    // nothing: keeping the event keeps the chunk sequence the events produced, and dropping it
    // would hide a delta the caller's protocol asked for.
    let mut events = events();
    events.push(StreamEvent::TextDelta(String::new()));
    events.push(StreamEvent::Finish(FinishReason::Stop));
    let chunks = chunks(&encode_stream(&events));
    assert_eq!(chunks[3]["choices"][0]["delta"]["content"], "");
    assert!(
        chunks[3]["choices"][0]["finish_reason"].is_null(),
        "an empty delta is not the finish chunk"
    );
}

#[test]
fn tool_call_fragments_carry_the_index_and_only_the_first_names_the_call() {
    let mut events = events();
    events.push(StreamEvent::ToolCallStart {
        id: ToolCallId::try_new("call_a").expect("a valid call id"),
        name: ToolName::try_new("weather").expect("a valid tool name"),
        index: 0,
    });
    events.push(StreamEvent::ToolCallDelta {
        id: ToolCallId::try_new("call_a").expect("a valid call id"),
        arguments: "{\"city\":".to_owned(),
        index: 0,
    });
    events.push(StreamEvent::ToolCallDelta {
        id: ToolCallId::try_new("call_a").expect("a valid call id"),
        arguments: "\"Paris\"}".to_owned(),
        index: 0,
    });
    events.push(StreamEvent::ToolCallEnd {
        id: ToolCallId::try_new("call_a").expect("a valid call id"),
        index: 0,
    });
    events.push(StreamEvent::ToolCallStart {
        id: ToolCallId::try_new("call_b").expect("a valid call id"),
        name: ToolName::try_new("time").expect("a valid tool name"),
        index: 1,
    });
    events.push(StreamEvent::Finish(FinishReason::ToolCalls));
    let chunks = chunks(&encode_stream(&events));

    let start = &chunks[3]["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(start["index"], 0);
    assert_eq!(start["id"], "call_a");
    assert_eq!(start["type"], "function");
    assert_eq!(start["function"]["name"], "weather");
    assert_eq!(start["function"]["arguments"], "");
    let fragment = &chunks[4]["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(fragment["index"], 0);
    assert!(
        fragment.get("id").is_none(),
        "a later fragment does not repeat the call id"
    );
    assert_eq!(fragment["function"]["arguments"], "{\"city\":");
    assert_eq!(
        chunks.len(),
        8,
        "the end of a call writes no chunk of its own"
    );
    assert_eq!(
        chunks[6]["choices"][0]["delta"]["tool_calls"][0]["index"], 1,
        "the second call keeps its own position"
    );
    assert_eq!(chunks[7]["choices"][0]["finish_reason"], "tool_calls");
}

#[test]
fn usage_is_written_after_the_finish_reason() {
    let mut events = events();
    events.push(StreamEvent::Usage(Usage::reported(
        UsageCounts::new()
            .with(TokenKind::Input, 10)
            .with(TokenKind::Output, 2),
    )));
    events.push(StreamEvent::Finish(FinishReason::Stop));
    let stream = encode_stream(&events);
    let chunks = chunks(&stream);
    assert_eq!(
        chunks[3]["choices"][0]["finish_reason"], "stop",
        "the finish chunk comes first"
    );
    assert_eq!(
        chunks[4]["choices"],
        serde_json::json!([]),
        "the usage chunk carries no choice"
    );
    assert_eq!(chunks[4]["usage"]["prompt_tokens"], 10);
    assert_eq!(chunks[4]["usage"]["total_tokens"], 12);
    assert_eq!(frames(&stream).last().map(String::as_str), Some("[DONE]"));
}

#[test]
fn measured_only_usage_is_not_streamed() {
    let mut events = events();
    events.push(StreamEvent::Usage(Usage::measured(
        UsageCounts::new().with(TokenKind::Input, 10),
    )));
    events.push(StreamEvent::Finish(FinishReason::Stop));
    let chunks = chunks(&encode_stream(&events));
    assert_eq!(chunks.len(), 4, "no usage chunk is written");
    assert!(chunks.iter().all(|chunk| chunk.get("usage").is_none()));
}

#[test]
fn a_stream_that_ended_early_is_terminated() {
    let stream = encode_stream(&events());
    assert_eq!(
        frames(&stream).last().map(String::as_str),
        Some("[DONE]"),
        "a body that ended without a finish event still leaves a terminated stream"
    );
}

#[test]
fn an_error_ends_the_stream_without_a_terminator() {
    let mut events = events();
    events.push(StreamEvent::Error(
        CanonicalError::try_new(
            ErrorCategory::Upstream,
            "provider_overloaded",
            "the provider is temporarily overloaded",
            503,
        )
        .expect("a valid canonical error"),
    ));
    events.push(StreamEvent::TextDelta("after the failure".to_owned()));
    let stream = encode_stream(&events);
    let payloads = frames(&stream);
    assert_eq!(payloads.len(), 4, "start, two deltas and the error");
    assert_ne!(payloads.last().map(String::as_str), Some("[DONE]"));
    let error = &chunks(&stream)[3];
    assert_eq!(error["error"]["code"], "provider_overloaded");
    assert_eq!(error["error"]["type"], "server_error");
    assert!(
        !payloads
            .iter()
            .any(|payload| payload.contains("after the failure")),
        "nothing follows the failure"
    );
}

#[test]
fn an_event_before_the_start_event_is_refused() {
    let mut encoder = codec().stream_encoder();
    let error = encoder
        .push(&StreamEvent::TextDelta("hello".to_owned()))
        .expect_err("a chunk without an identity cannot be framed");
    assert!(
        matches!(&error, nico_llm::CodecError::Encode { reason, .. }
            if reason.contains("start")),
        "the refusal explains why: {error:?}"
    );
}

#[test]
fn usage_that_arrives_after_the_finish_reason_is_still_written() {
    let mut events = events();
    events.push(StreamEvent::Usage(Usage::reported(
        UsageCounts::new()
            .with(TokenKind::Input, 3)
            .with(TokenKind::Output, 1),
    )));
    events.push(StreamEvent::Finish(FinishReason::Stop));
    // The provider reported the usage in the chunk that follows its finish event.
    let chunks = chunks(&encode_stream(&events));
    assert_eq!(chunks.len(), 5);
    assert_eq!(chunks[3]["choices"][0]["finish_reason"], "stop");
    assert_eq!(chunks[4]["usage"]["total_tokens"], 4);
    assert_eq!(
        chunks[4]["choices"],
        serde_json::json!([]),
        "the usage chunk carries no choice"
    );
}
