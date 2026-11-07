//! Folding `output[]` items into one assistant turn, and the item shapes this adapter refuses.
//!
//! The envelope around the items (`id`, `model`, `status`, usage, extensions) is asserted in
//! [`super`].

use nico_llm::FinishReason;
use serde_json::{Value, json};

use super::{document, with_item};
use crate::support::{decode_err, decoded};

#[test]
fn a_tool_call_makes_the_answer_a_tool_call_finish() {
    let body = with_item(json!({
        "type": "function_call",
        "id": "fc_1",
        "call_id": "call_1",
        "name": "lookup",
        "arguments": "{\"q\":\"x\"}",
        "status": "completed",
    }));
    let response = decoded(&body);
    let choice = &response.choices()[0];
    assert_eq!(
        choice.finish_reason(),
        FinishReason::ToolCalls,
        "a completed answer that asked for a tool stopped for that reason"
    );
    let calls = choice.turn().tool_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id().as_str(), "call_1");
    assert_eq!(calls[0].name().as_str(), "lookup");
    assert_eq!(calls[0].arguments().as_value(), &json!({"q": "x"}));
}

#[test]
fn a_call_with_no_arguments_is_the_empty_object() {
    for arguments in [json!(null), json!(""), json!("  ")] {
        let mut item = json!({
            "type": "function_call",
            "call_id": "call_1",
            "name": "lookup",
        });
        if !arguments.is_null() {
            item["arguments"] = arguments;
        }
        let response = decoded(&with_item(item));
        assert_eq!(
            response.choices()[0].turn().tool_calls()[0]
                .arguments()
                .as_value(),
            &json!({}),
            "a provider that sends no arguments means \"no arguments\""
        );
    }
}

#[test]
fn a_call_with_malformed_arguments_is_refused() {
    let body = with_item(json!({
        "type": "function_call",
        "call_id": "call_1",
        "name": "lookup",
        "arguments": "{not json",
    }));
    assert!(matches!(
        decode_err(200, &body),
        nico_llm::ProviderError::Malformed { .. }
    ));
}

#[test]
fn a_call_without_its_pairing_key_is_refused() {
    let body = with_item(json!({
        "type": "function_call",
        "name": "lookup",
        "arguments": "{}",
    }));
    assert!(matches!(
        decode_err(200, &body),
        nico_llm::ProviderError::Malformed { .. }
    ));
}

#[test]
fn several_text_parts_of_a_message_are_one_visible_text() {
    let body = with_item(json!({
        "type": "message",
        "role": "assistant",
        "content": [
            {"type": "output_text", "text": "a", "annotations": []},
            {"type": "output_text", "text": "b", "annotations": []},
        ],
    }));
    assert_eq!(
        decoded(&body).choices()[0].turn().content().to_vec(),
        vec![nico_llm::ContentPart::text("ab")]
    );
}

#[test]
fn a_message_whose_parts_are_all_empty_says_nothing() {
    let body = with_item(json!({
        "type": "message",
        "role": "assistant",
        "content": [{"type": "output_text", "text": "", "annotations": []}],
    }));
    let response = decoded(&body);
    assert!(
        response.choices()[0].turn().content().is_empty(),
        "a zero-length canonical part would say the same thing in a shape nobody asked for"
    );
}

#[test]
fn a_message_that_is_not_an_assistant_turn_is_refused() {
    let body = with_item(json!({
        "type": "message",
        "role": "user",
        "content": [{"type": "output_text", "text": "hi"}],
    }));
    assert!(matches!(
        decode_err(200, &body),
        nico_llm::ProviderError::Malformed { .. }
    ));
}

#[test]
fn a_refusal_part_is_refused_rather_than_hidden() {
    let body = with_item(json!({
        "type": "message",
        "role": "assistant",
        "content": [{"type": "refusal", "refusal": "I cannot help with that"}],
    }));
    assert!(matches!(
        decode_err(200, &body),
        nico_llm::ProviderError::Malformed { .. }
    ));
}

#[test]
fn a_citation_is_refused_rather_than_stripped() {
    // Citations come from a built-in tool this adapter never requests, so the answer holding one
    // means the route served something it could not have asked for.
    let body = with_item(json!({
        "type": "message",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": "hi",
            "annotations": [{"type": "url_citation", "url": "https://x.test"}],
        }],
    }));
    assert!(matches!(
        decode_err(200, &body),
        nico_llm::ProviderError::Malformed { .. }
    ));
}

#[test]
fn an_unknown_content_part_is_refused() {
    let body = with_item(json!({
        "type": "message",
        "role": "assistant",
        "content": [{"type": "future_part", "text": "hi"}],
    }));
    assert!(matches!(
        decode_err(200, &body),
        nico_llm::ProviderError::Malformed { .. }
    ));
}

#[test]
fn a_reasoning_summary_is_rendered_without_a_signature() {
    let body = with_item(json!({
        "type": "reasoning",
        "id": "rs_1",
        "summary": [{"type": "summary_text", "text": "thought about it"}],
        "encrypted_content": "opaque-handle",
    }));
    let response = decoded(&body);
    let reasoning = response.choices()[0]
        .turn()
        .reasoning()
        .expect("the summary is part of the answer");
    assert_eq!(reasoning.text(), "thought about it");
    assert_eq!(
        reasoning.signature(),
        None,
        "the replay handles cannot be carried, so the item cannot be sent back"
    );
}

#[test]
fn an_empty_reasoning_summary_produces_no_reasoning() {
    let body = with_item(json!({
        "type": "reasoning",
        "id": "rs_1",
        "summary": [],
    }));
    assert!(decoded(&body).choices()[0].turn().reasoning().is_none());
}

#[test]
fn two_reasoning_items_are_refused_rather_than_merged() {
    let mut document: Value = serde_json::from_str(&document()).expect("the fixture is JSON");
    document["output"] = json!([
        {"type": "reasoning", "summary": [{"type": "summary_text", "text": "a"}]},
        {"type": "reasoning", "summary": [{"type": "summary_text", "text": "b"}]},
    ]);
    assert!(matches!(
        decode_err(200, &document.to_string()),
        nico_llm::ProviderError::Malformed { .. }
    ));
}

#[test]
fn an_unknown_summary_part_is_refused() {
    let body = with_item(json!({
        "type": "reasoning",
        "summary": [{"type": "reasoning_text", "text": "a"}],
    }));
    assert!(matches!(
        decode_err(200, &body),
        nico_llm::ProviderError::Malformed { .. }
    ));
}

#[test]
fn an_unknown_output_item_type_is_refused_rather_than_skipped() {
    // An unknown item can carry content the caller would otherwise see.
    let body = with_item(json!({"type": "web_search_call", "id": "ws_1"}));
    assert!(matches!(
        decode_err(200, &body),
        nico_llm::ProviderError::Malformed { .. }
    ));
}

#[test]
fn an_unknown_member_inside_an_output_item_is_refused() {
    let body = with_item(json!({
        "type": "message",
        "role": "assistant",
        "content": [{"type": "output_text", "text": "hi"}],
        "future_field": 1,
    }));
    assert!(matches!(
        decode_err(200, &body),
        nico_llm::ProviderError::Malformed { .. }
    ));
}
