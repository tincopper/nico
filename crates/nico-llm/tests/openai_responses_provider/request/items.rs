//! The `input` list: one canonical item per wire item, and the shapes this dialect cannot carry.
//!
//! The document around the list (model, instructions, tools, sampling, format, `store`) is asserted in
//! [`super`].

use nico_llm::{Capability, ContentPart, Item, Reasoning, Role};
use serde_json::json;

use super::item;
use crate::support::{
    assistant, call, encoded, failed_result, inline_image, message, reasoning, remote_image,
    request, result, tool_result,
};

#[test]
fn an_assistant_turn_is_replayed_with_output_text_parts() {
    // The same spelling this dialect's `output[]` uses for exactly this content.
    let body = encoded(&request(vec![
        Item::Message(nico_llm::Message::user("hi")),
        assistant("hello"),
    ]));
    assert_eq!(
        item(&body, 1),
        &json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "hello"}],
        })
    );
}

#[test]
fn an_image_is_an_input_image_part_in_a_user_turn() {
    let body = encoded(&request(vec![Item::Message(
        nico_llm::Message::try_new(
            Role::User,
            vec![
                ContentPart::text("what is this?"),
                remote_image("https://images.example.test/a.png"),
            ],
        )
        .expect("a valid message"),
    )]));
    assert_eq!(
        item(&body, 0)["content"][1],
        json!({"type": "input_image", "image_url": "https://images.example.test/a.png"}),
        "a remote image is addressed, not uploaded"
    );
}

#[test]
fn an_inline_image_becomes_a_data_url() {
    let body = encoded(&request(vec![Item::Message(
        nico_llm::Message::try_new(Role::User, vec![inline_image("image/png", b"bytes")])
            .expect("a valid message"),
    )]));
    assert_eq!(
        item(&body, 0)["content"][0],
        json!({"type": "input_image", "image_url": "data:image/png;base64,Ynl0ZXM="})
    );
}

#[test]
fn an_image_outside_a_user_turn_is_refused() {
    // An assistant turn's content is output text; the dialect has no image part for it.
    let request = request(vec![
        Item::Message(nico_llm::Message::user("hi")),
        message(Role::Assistant, vec![remote_image("https://x.test/a.png")]),
    ]);
    assert!(matches!(
        crate::support::encode_err(&request),
        nico_llm::ProviderError::UnsupportedField { field, .. } if field == "message.content"
    ));
}

#[test]
fn an_instruction_turn_carrying_an_image_is_refused() {
    let request = request(vec![
        message(Role::System, vec![remote_image("https://x.test/a.png")]),
        Item::Message(nico_llm::Message::user("hi")),
    ]);
    assert!(matches!(
        crate::support::encode_err(&request),
        nico_llm::ProviderError::UnsupportedField { field, .. } if field == "message.content"
    ));
}

#[test]
fn a_file_part_is_an_undeclared_capability() {
    let request = request(vec![Item::Message(
        nico_llm::Message::try_new(
            Role::User,
            vec![ContentPart::File {
                media_type: "application/pdf".to_owned(),
                data: b"bytes".to_vec(),
                name: None,
            }],
        )
        .expect("a valid message"),
    )]);
    assert!(matches!(
        crate::support::encode_err(&request),
        nico_llm::ProviderError::Unsupported {
            feature: Capability::Files,
            ..
        }
    ));
}

#[test]
fn a_tool_call_is_one_function_call_item() {
    let body = encoded(&request(vec![
        Item::Message(nico_llm::Message::user("look it up")),
        call("call_1", "lookup", json!({"q": "x"})),
    ]));
    assert_eq!(
        item(&body, 1),
        &json!({
            "type": "function_call",
            "call_id": "call_1",
            "name": "lookup",
            "arguments": "{\"q\":\"x\"}",
        }),
        "the arguments travel as the JSON string the dialect carries, and the provider's item id \
         is not echoed"
    );
}

#[test]
fn a_tool_result_is_one_function_call_output_item() {
    let body = encoded(&request(vec![
        Item::Message(nico_llm::Message::user("look it up")),
        call("call_1", "lookup", json!({})),
        result("call_1", "found"),
    ]));
    assert_eq!(
        item(&body, 2),
        &json!({"type": "function_call_output", "call_id": "call_1", "output": "found"})
    );
}

#[test]
fn several_text_parts_of_a_tool_result_are_joined() {
    let body = encoded(&request(vec![
        Item::Message(nico_llm::Message::user("look it up")),
        tool_result(
            "call_1",
            vec![ContentPart::text("a"), ContentPart::text("b")],
            false,
        ),
    ]));
    assert_eq!(item(&body, 1)["output"], "ab");
}

#[test]
fn a_failed_tool_result_is_refused() {
    // `function_call_output` has no field saying the call failed; writing the text alone would tell
    // the model the tool succeeded.
    let request = request(vec![
        Item::Message(nico_llm::Message::user("look it up")),
        failed_result("call_1", "boom"),
    ]);
    assert!(matches!(
        crate::support::encode_err(&request),
        nico_llm::ProviderError::UnsupportedField { field, .. } if field == "tool_result.is_error"
    ));
}

#[test]
fn an_image_inside_a_tool_result_is_refused() {
    let request = request(vec![
        Item::Message(nico_llm::Message::user("look it up")),
        tool_result("call_1", vec![inline_image("image/png", b"x")], false),
    ]);
    assert!(matches!(
        crate::support::encode_err(&request),
        nico_llm::ProviderError::UnsupportedField { field, .. } if field == "tool_result.content"
    ));
}

#[test]
fn a_reasoning_item_is_an_undeclared_capability() {
    // Replaying one needs both handles (`id` and `encrypted_content`); the canonical model has one
    // opaque signature, so the item cannot be sent back at all.
    let signed =
        Reasoning::try_new("think", Some("signature".to_owned())).expect("valid reasoning");
    for item in [reasoning("think"), Item::Reasoning(signed)] {
        let request = request(vec![item]);
        assert!(matches!(
            crate::support::encode_err(&request),
            nico_llm::ProviderError::Unsupported {
                feature: Capability::Reasoning,
                ..
            }
        ));
    }
}
