//! Tool calls: assembly across fragments, parallel calls addressed by index, and the refusals.

use nico_llm::{FinishReason, ProviderError, StreamEvent, ToolCallId, ToolName};
use serde_json::json;

use super::{
    block_stop, completed, events, finish, json_event, push_err, reported, start, start_event,
};

/// The event that opens a tool call, whose arguments arrive separately.
fn tool_block(index: u32, id: &str, name: &str) -> String {
    json_event(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": index,
            "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}},
        }),
    )
}

/// An argument fragment of a tool call.
fn arguments(index: u32, partial: &str) -> String {
    json_event(
        "content_block_delta",
        json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "input_json_delta", "partial_json": partial},
        }),
    )
}

/// One tool call's canonical identity.
fn call(id: &str, name: &str) -> (ToolCallId, ToolName) {
    (
        ToolCallId::try_new(id).expect("a valid id"),
        ToolName::try_new(name).expect("a valid name"),
    )
}

#[test]
fn a_tool_call_is_assembled_across_fragments() {
    let (id, name) = call("toolu_1", "lookup");
    let events = events(&[
        start(),
        tool_block(0, "toolu_1", "lookup"),
        arguments(0, "{\"q\":"),
        arguments(0, "\"x\"}"),
        block_stop(0),
        finish("tool_use"),
    ]);
    assert_eq!(
        events,
        vec![
            start_event(),
            reported(10, 1),
            StreamEvent::ToolCallStart {
                id: id.clone(),
                name,
                index: 0,
            },
            StreamEvent::ToolCallDelta {
                id: id.clone(),
                arguments: "{\"q\":".to_owned(),
                index: 0,
            },
            StreamEvent::ToolCallDelta {
                id: id.clone(),
                arguments: "\"x\"}".to_owned(),
                index: 0,
            },
            StreamEvent::ToolCallEnd { id, index: 0 },
            completed(5),
            StreamEvent::Finish(FinishReason::ToolCalls),
        ]
    );
}

#[test]
fn parallel_calls_are_addressed_by_their_index() {
    // The protocol interleaves the blocks of two calls; canonical events carry the index the peer
    // chose, so a client can address a call before the id it already received is echoed again.
    let events = events(&[
        start(),
        tool_block(0, "toolu_1", "lookup"),
        tool_block(1, "toolu_2", "fetch"),
        arguments(1, "{}"),
        arguments(0, "{}"),
        block_stop(0),
        block_stop(1),
        finish("tool_use"),
    ]);
    let starts: Vec<u32> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ToolCallStart { index, .. } => Some(*index),
            _ => None,
        })
        .collect();
    assert_eq!(starts, [0, 1]);
    let deltas: Vec<(&str, u32)> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ToolCallDelta {
                id,
                arguments,
                index,
            } => (arguments == "{}").then_some((id.as_str(), *index)),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, [("toolu_2", 1), ("toolu_1", 0)]);
    let ends: Vec<u32> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ToolCallEnd { index, .. } => Some(*index),
            _ => None,
        })
        .collect();
    assert_eq!(ends, [0, 1]);
}

#[test]
fn a_tool_call_started_with_arguments_is_refused() {
    // The arguments are what the deltas assemble; a start that already carries them would be
    // arguments outside a delta, and the event stream cannot order them.
    let filled = json_event(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {
                "type": "tool_use",
                "id": "toolu_1",
                "name": "lookup",
                "input": {"q": "x"},
            },
        }),
    );
    assert!(matches!(
        push_err(&[start(), filled]),
        ProviderError::Malformed { .. }
    ));
}

#[test]
fn a_tool_call_without_an_id_or_name_is_refused() {
    for content_block in [
        json!({"type": "tool_use", "name": "lookup", "input": {}}),
        json!({"type": "tool_use", "id": "toolu_1", "input": {}}),
    ] {
        let started = json_event(
            "content_block_start",
            json!({"type": "content_block_start", "index": 0, "content_block": content_block}),
        );
        assert!(matches!(
            push_err(&[start(), started]),
            ProviderError::Malformed { .. }
        ));
    }
}

#[test]
fn a_tool_block_that_is_never_closed_is_refused_at_the_finish() {
    let error = push_err(&[
        start(),
        tool_block(0, "toolu_1", "lookup"),
        arguments(0, "{}"),
        finish("tool_use"),
    ]);
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
}
