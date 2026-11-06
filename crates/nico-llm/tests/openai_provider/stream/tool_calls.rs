//! Tool calls: assembly across fragments, parallel calls addressed by index, and the refusals.

use nico_llm::{FinishReason, ProviderError, StreamEvent};
use serde_json::json;

use super::{done, events, finish, push_err, start, start_event, tool_call};

#[test]
fn a_tool_call_is_assembled_across_fragments() {
    let events = events(&[
        start(),
        tool_call(json!({
            "index": 0,
            "id": "call_1",
            "type": "function",
            "function": {"name": "lookup", "arguments": ""},
        })),
        tool_call(json!({"index": 0, "function": {"arguments": "{\"q\":"}})),
        tool_call(json!({"index": 0, "function": {"arguments": "\"x\"}"}})),
        finish("tool_calls"),
        done(),
    ]);
    let id = nico_llm::ToolCallId::try_new("call_1").expect("a valid id");
    let name = nico_llm::ToolName::try_new("lookup").expect("a valid name");
    assert_eq!(
        events,
        vec![
            start_event(),
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
            StreamEvent::Finish(FinishReason::ToolCalls),
        ]
    );
}

#[test]
fn two_parallel_calls_are_addressed_by_index() {
    let events = events(&[
        start(),
        tool_call(json!({
            "index": 0,
            "id": "call_0",
            "type": "function",
            "function": {"name": "first", "arguments": ""},
        })),
        tool_call(json!({
            "index": 1,
            "id": "call_1",
            "type": "function",
            "function": {"name": "second", "arguments": ""},
        })),
        tool_call(json!({"index": 1, "function": {"arguments": "{\"b\":1}"}})),
        tool_call(json!({"index": 0, "function": {"arguments": "{\"a\":1}"}})),
        finish("tool_calls"),
        done(),
    ]);
    let first = nico_llm::ToolCallId::try_new("call_0").expect("a valid id");
    let second = nico_llm::ToolCallId::try_new("call_1").expect("a valid id");
    assert_eq!(
        events,
        vec![
            start_event(),
            StreamEvent::ToolCallStart {
                id: first.clone(),
                name: nico_llm::ToolName::try_new("first").expect("a valid name"),
                index: 0,
            },
            StreamEvent::ToolCallStart {
                id: second.clone(),
                name: nico_llm::ToolName::try_new("second").expect("a valid name"),
                index: 1,
            },
            StreamEvent::ToolCallDelta {
                id: second.clone(),
                arguments: "{\"b\":1}".to_owned(),
                index: 1,
            },
            StreamEvent::ToolCallDelta {
                id: first.clone(),
                arguments: "{\"a\":1}".to_owned(),
                index: 0,
            },
            StreamEvent::ToolCallEnd {
                id: first,
                index: 0,
            },
            StreamEvent::ToolCallEnd {
                id: second,
                index: 1,
            },
            StreamEvent::Finish(FinishReason::ToolCalls),
        ],
        "a stop closes every open call in index order"
    );
}

#[test]
fn an_unknown_tool_index_is_refused() {
    let error = push_err(&[
        start(),
        tool_call(json!({"index": 3, "function": {"arguments": "{}"}})),
    ]);
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
}

#[test]
fn a_tool_call_fragment_may_not_change_its_id() {
    let opened = tool_call(json!({
        "index": 0,
        "id": "call_1",
        "type": "function",
        "function": {"name": "lookup", "arguments": ""},
    }));
    let changed = tool_call(json!({"index": 0, "id": "call_2", "function": {"arguments": "{}"}}));
    let error = push_err(&[start(), opened, changed]);
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
}
