//! The conversation: assistant runs, grouped tool results and content blocks.
//!
//! The canonical model keeps text and tool calls as separate items; this protocol carries them in one
//! message, and a tool result belongs to a user turn even after an assistant one. These tests pin the
//! folding, the grouping and the refusals that belong to it.

use nico_llm::{Capability, ContentPart, Item, Message, Role};
use serde_json::json;

use super::wire_message;
use crate::support::{
    assistant, call, encode_err, encoded, failed_result, inline_image, message, remote_image,
    request, result, tool_result,
};

#[test]
fn reasoning_text_and_tool_calls_share_one_assistant_turn() {
    let request = request(vec![
        message(Role::User, vec![ContentPart::text("ask")]),
        assistant("thinking out loud"),
        call("toolu_1", "lookup", json!({"q": "x"})),
        call("toolu_2", "fetch", json!({})),
    ]);
    let body = encoded(&request);
    assert_eq!(body["messages"].as_array().expect("a list").len(), 2);
    assert_eq!(
        wire_message(&body, 1)["content"],
        json!([
            {"type": "text", "text": "thinking out loud"},
            {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {"q": "x"}},
            {"type": "tool_use", "id": "toolu_2", "name": "fetch", "input": {}},
        ]),
        "a tool call is a block of the assistant turn, and its arguments are an object here"
    );
}

#[test]
fn a_tool_result_opens_the_next_user_turn() {
    let request = request(vec![
        message(Role::User, vec![ContentPart::text("ask")]),
        call("toolu_1", "lookup", json!({})),
        result("toolu_1", "42"),
        message(Role::User, vec![ContentPart::text("thanks")]),
    ]);
    let body = encoded(&request);
    assert_eq!(
        wire_message(&body, 2)["content"],
        json!([
            {"type": "tool_result", "tool_use_id": "toolu_1", "content": "42"},
            {"type": "text", "text": "thanks"},
        ]),
        "the result leads the user turn that answers it"
    );
    assert_eq!(wire_message(&body, 2)["role"], "user");
}

#[test]
fn consecutive_tool_results_are_one_user_turn() {
    let request = request(vec![
        message(Role::User, vec![ContentPart::text("ask")]),
        call("toolu_1", "lookup", json!({})),
        call("toolu_2", "fetch", json!({})),
        result("toolu_1", "42"),
        result("toolu_2", "ok"),
        assistant("both done"),
    ]);
    let body = encoded(&request);
    assert_eq!(body["messages"].as_array().expect("a list").len(), 4);
    assert_eq!(
        wire_message(&body, 2)["content"],
        json!([
            {"type": "tool_result", "tool_use_id": "toolu_1", "content": "42"},
            {"type": "tool_result", "tool_use_id": "toolu_2", "content": "ok"},
        ])
    );
}

#[test]
fn a_failed_tool_result_keeps_its_flag() {
    // This protocol can express a failed result, so it is written rather than refused the way the
    // OpenAI adapter must.
    let request = request(vec![failed_result("toolu_1", "it broke")]);
    assert_eq!(
        encoded(&request)["messages"][0]["content"][0],
        json!({
            "type": "tool_result",
            "tool_use_id": "toolu_1",
            "content": "it broke",
            "is_error": true,
        })
    );
}

#[test]
fn a_tool_result_with_an_image_is_written_as_blocks() {
    let request = request(vec![tool_result(
        "toolu_1",
        vec![
            ContentPart::text("look"),
            remote_image("https://example.test/image.png"),
        ],
        false,
    )]);
    assert_eq!(
        encoded(&request)["messages"][0]["content"][0]["content"],
        json!([
            {"type": "text", "text": "look"},
            {
                "type": "image",
                "source": {"type": "url", "url": "https://example.test/image.png"},
            },
        ])
    );
}

#[test]
fn an_inline_image_becomes_a_base64_source() {
    let body = encoded(&request(vec![message(
        Role::User,
        vec![ContentPart::text("look"), inline_image("image/png", b"f")],
    )]));
    assert_eq!(
        body["messages"][0]["content"],
        json!([
            {"type": "text", "text": "look"},
            {
                "type": "image",
                "source": {"type": "base64", "media_type": "image/png", "data": "Zg=="},
            },
        ])
    );
}

#[test]
fn a_remote_image_is_sent_as_its_url() {
    let body = encoded(&request(vec![message(
        Role::User,
        vec![remote_image("https://example.test/image.png")],
    )]));
    assert_eq!(
        body["messages"][0]["content"][0],
        json!({
            "type": "image",
            "source": {"type": "url", "url": "https://example.test/image.png"},
        })
    );
}

#[test]
fn an_image_outside_a_user_turn_is_refused() {
    for (role, image) in [
        (Role::System, "https://e.test/system.png"),
        (Role::Developer, "https://e.test/developer.png"),
        (Role::Assistant, "https://e.test/assistant.png"),
    ] {
        let request = request(vec![message(role, vec![remote_image(image)])]);
        let error = encode_err(&request);
        assert!(
            matches!(
                &error,
                nico_llm::ProviderError::UnsupportedField { field, .. } if field == "message.content"
            ),
            "an image on a {role:?} message must name the field, got {error:?}"
        );
        assert!(
            !format!("{error:?} {error}").contains(image),
            "a refusal must not quote the value: {error}"
        );
    }
}

#[test]
fn a_file_part_is_refused_as_an_unsupported_capability() {
    let request = request(vec![message(
        Role::User,
        vec![ContentPart::File {
            media_type: "application/pdf".to_owned(),
            data: vec![1, 2, 3],
            name: None,
        }],
    )]);
    assert!(
        matches!(
            encode_err(&request),
            nico_llm::ProviderError::Unsupported {
                feature: Capability::Files,
                ..
            }
        ),
        "the adapter must not claim a file it cannot send"
    );
}

#[test]
fn a_conversation_that_does_not_open_with_a_user_turn_is_refused() {
    let request = request(vec![assistant("hello")]);
    assert!(
        matches!(
            encode_err(&request),
            nico_llm::ProviderError::UnsupportedField { field, .. }
                if field == "conversation[].role"
        ),
        "this protocol's first message must come from the caller"
    );
}

#[test]
fn a_conversation_of_instructions_only_is_refused() {
    let request = request(vec![message(
        Role::System,
        vec![ContentPart::text("be brief")],
    )]);
    assert!(
        matches!(
            encode_err(&request),
            nico_llm::ProviderError::UnsupportedField { field, .. } if field == "conversation"
        ),
        "the protocol requires at least one message"
    );
}

#[test]
fn the_message_order_survives_the_folding() {
    let request = request(vec![
        message(Role::System, vec![ContentPart::text("be brief")]),
        Item::Message(Message::user("ask")),
        assistant("call it"),
        call("toolu_1", "lookup", json!({})),
        result("toolu_1", "42"),
        Item::Message(Message::user("thanks")),
    ]);
    let body = encoded(&request);
    let roles: Vec<&str> = body["messages"]
        .as_array()
        .expect("a message list")
        .iter()
        .map(|message| message["role"].as_str().expect("a role"))
        .collect();
    assert_eq!(roles, ["user", "assistant", "user"]);
}
