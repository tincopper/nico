//! The tool-call assembler: announcement, argument fragments, ending and its refusals.
//!
//! The order of a text answer, the framing events and the failures are asserted in [`super`].

use nico_llm::{
    FinishReason, StreamEvent, TokenKind, provider::openai_responses::MAX_OUTPUT_ITEMS,
};
use serde_json::json;

use super::{MODEL, completed, created, usage};
use crate::support::{events, json_event, push_err};

/// The frames of a streamed tool call.
fn call_stream() -> Vec<String> {
    vec![
        created(),
        json_event(json!({
            "type": "response.output_item.added",
            "sequence_number": 1,
            "output_index": 0,
            "item": {
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "lookup",
                "arguments": "",
                "status": "in_progress",
            },
        })),
        json_event(json!({
            "type": "response.function_call_arguments.delta",
            "sequence_number": 2,
            "item_id": "fc_1",
            "output_index": 0,
            "delta": "{\"q\":",
        })),
        json_event(json!({
            "type": "response.function_call_arguments.delta",
            "sequence_number": 3,
            "item_id": "fc_1",
            "output_index": 0,
            "delta": "\"x\"}",
        })),
        json_event(json!({
            "type": "response.function_call_arguments.done",
            "sequence_number": 4,
            "item_id": "fc_1",
            "output_index": 0,
            "arguments": "{\"q\":\"x\"}",
        })),
        completed(usage()),
    ]
}

#[test]
fn a_tool_call_starts_with_its_first_fragment_and_ends_before_the_finish() {
    let events = events(&call_stream());
    let id = nico_llm::ToolCallId::try_new("call_1").expect("a valid call id");
    assert_eq!(
        events,
        vec![
            StreamEvent::Start {
                id: nico_llm::ResponseId::try_new("resp_1").expect("a valid id"),
                model: nico_llm::ModelId::try_new(MODEL).expect("a valid model"),
            },
            StreamEvent::ToolCallStart {
                id: id.clone(),
                name: nico_llm::ToolName::try_new("lookup").expect("a valid name"),
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
            StreamEvent::Usage(nico_llm::Usage::reported(
                nico_llm::UsageCounts::new()
                    .with(TokenKind::Input, 10)
                    .with(TokenKind::Output, 5)
                    .with(TokenKind::CacheRead, 3)
                    .with(TokenKind::Reasoning, 2)
            )),
            StreamEvent::Finish(FinishReason::ToolCalls),
        ],
        "the `done` event repeats the whole argument string and must not write it twice"
    );
}

#[test]
fn a_call_that_never_streamed_a_fragment_is_written_from_its_done_event() {
    let chunks = vec![
        created(),
        json_event(json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "function_call", "call_id": "call_1", "name": "lookup"},
        })),
        json_event(json!({
            "type": "response.function_call_arguments.done",
            "output_index": 0,
            "arguments": "{}",
        })),
        completed(json!(null)),
    ];
    let id = nico_llm::ToolCallId::try_new("call_1").expect("a valid call id");
    assert_eq!(
        events(&chunks)[1..4],
        [
            StreamEvent::ToolCallStart {
                id: id.clone(),
                name: nico_llm::ToolName::try_new("lookup").expect("a valid name"),
                index: 0,
            },
            StreamEvent::ToolCallDelta {
                id: id.clone(),
                arguments: "{}".to_owned(),
                index: 0,
            },
            StreamEvent::ToolCallEnd { id, index: 0 },
        ]
    );
}

#[test]
fn an_open_call_is_closed_by_the_completed_event() {
    // A call whose `done` event never arrives still ends: the client must not wait for a call the
    // answer stopped producing.
    let chunks = vec![
        created(),
        json_event(json!({
            "type": "response.output_item.added",
            "output_index": 3,
            "item": {"type": "function_call", "call_id": "call_9", "name": "lookup"},
        })),
        json_event(json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 3,
            "delta": "{}",
        })),
        completed(json!(null)),
    ];
    let events = events(&chunks);
    assert_eq!(
        events[3],
        StreamEvent::ToolCallEnd {
            id: nico_llm::ToolCallId::try_new("call_9").expect("a valid call id"),
            index: 3,
        },
        "the call is closed before the finish event"
    );
    assert_eq!(events[4], StreamEvent::Finish(FinishReason::ToolCalls));
}

#[test]
fn an_argument_fragment_for_an_unannounced_call_is_refused() {
    let chunks = vec![
        created(),
        json_event(json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 4,
            "delta": "{}",
        })),
    ];
    assert!(matches!(
        push_err(&chunks),
        nico_llm::ProviderError::Malformed { .. }
    ));
}

#[test]
fn the_decoder_refuses_more_open_calls_than_it_serves() {
    // The peer picks every output index, so the bound is the adapter's, not the provider's.
    let mut chunks = vec![created()];
    for index in 0..=MAX_OUTPUT_ITEMS {
        chunks.push(json_event(json!({
            "type": "response.output_item.added",
            "output_index": index,
            "item": {
                "type": "function_call",
                "call_id": format!("call_{index}"),
                "name": "lookup",
            },
        })));
    }
    assert!(matches!(
        push_err(&chunks),
        nico_llm::ProviderError::Malformed { .. }
    ));
}

#[test]
fn a_reused_output_index_is_refused() {
    let chunks = vec![
        created(),
        json_event(json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "function_call", "call_id": "a", "name": "lookup"},
        })),
        json_event(json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "function_call", "call_id": "b", "name": "lookup"},
        })),
    ];
    assert!(matches!(
        push_err(&chunks),
        nico_llm::ProviderError::Malformed { .. }
    ));
}
