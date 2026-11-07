//! The conversation as `contents[]`: turns, runs, tool calls, results and their refusals.

use nico_llm::{ContentPart, Item, Message, ProviderError, Role};
use serde_json::json;

use super::field;
use crate::support::{
    assistant, call, encode_err, encoded, failed_result, inline_image, message, one, reasoning,
    remote_image, request, result, signed_reasoning, tool_result, user_request,
};

#[test]
fn a_user_turn_is_one_user_content_entry() {
    let document = encoded(&user_request("hello"));
    assert_eq!(
        document["contents"],
        json!([{"role": "user", "parts": [{"text": "hello"}]}])
    );
}

#[test]
fn an_assistant_turn_is_one_model_content_entry() {
    let document = encoded(&request(vec![
        Item::Message(Message::user("question")),
        assistant("answer"),
    ]));
    assert_eq!(
        document["contents"],
        json!([
            {"role": "user", "parts": [{"text": "question"}]},
            {"role": "model", "parts": [{"text": "answer"}]}
        ])
    );
}

#[test]
fn a_run_of_assistant_items_is_one_model_turn() {
    // This dialect's `contents[]` is a turn list, so a second assistant message would be a turn the
    // provider did not produce: the run is folded into one.
    let document = encoded(&request(vec![
        Item::Message(Message::user("question")),
        assistant("first"),
        assistant(" second"),
    ]));
    assert_eq!(
        document["contents"],
        json!([
            {"role": "user", "parts": [{"text": "question"}]},
            {"role": "model", "parts": [{"text": "first"}, {"text": " second"}]}
        ])
    );
}

#[test]
fn an_instruction_after_the_conversation_started_is_refused() {
    let request = request(vec![
        Item::Message(Message::user("hello")),
        message(Role::System, vec![ContentPart::text("be brief")]),
    ]);
    assert_eq!(field(encode_err(&request)), "conversation[].role");
}

#[test]
fn an_instruction_without_a_conversation_is_refused() {
    // The dialect requires at least one turn; instructions alone are not a conversation.
    let request = request(vec![message(
        Role::System,
        vec![ContentPart::text("be brief")],
    )]);
    assert_eq!(field(encode_err(&request)), "conversation");
}

#[test]
fn a_conversation_that_does_not_open_with_a_user_turn_is_refused() {
    let request = request(vec![assistant("I spoke first")]);
    assert_eq!(field(encode_err(&request)), "conversation[].role");
}

#[test]
fn a_tool_call_is_a_function_call_part_of_the_model_turn() {
    let request = request(vec![
        Item::Message(Message::user("weather?")),
        call("call-1", "weather", json!({"city": "Paris"})),
    ]);
    let document = encoded(&request);
    assert_eq!(
        document["contents"][1],
        json!({
            "role": "model",
            "parts": [{
                "functionCall": {"id": "call-1", "name": "weather", "args": {"city": "Paris"}}
            }]
        })
    );
}

#[test]
fn a_tool_result_names_the_call_it_answers() {
    // The canonical result carries only the call id; this dialect's `functionResponse` requires the
    // function's name, so the name comes from the call in the same conversation.
    let request = request(vec![
        Item::Message(Message::user("weather?")),
        call("call-1", "weather", json!({})),
        result("call-1", "sunny"),
    ]);
    let document = encoded(&request);
    assert_eq!(
        document["contents"][2],
        json!({
            "role": "user",
            "parts": [{
                "functionResponse": {
                    "id": "call-1",
                    "name": "weather",
                    "response": {"content": "sunny"}
                }
            }]
        })
    );
}

#[test]
fn consecutive_results_are_one_user_turn_and_their_text_is_joined() {
    let request = request(vec![
        Item::Message(Message::user("two tools?")),
        call("a", "one", json!({})),
        call("b", "two", json!({})),
        tool_result(
            "a",
            vec![ContentPart::text("line 1"), ContentPart::text("line 2")],
            false,
        ),
        result("b", "second"),
    ]);
    let document = encoded(&request);
    let turn = &document["contents"][2];
    assert_eq!(turn["role"], "user");
    assert_eq!(
        turn["parts"][0]["functionResponse"]["response"],
        json!({"content": "line 1\nline 2"}),
        "the canonical parts are joined into the one string this dialect's object carries"
    );
    assert_eq!(turn["parts"][1]["functionResponse"]["name"], "two");
}

#[test]
fn a_result_whose_call_is_not_in_the_conversation_is_refused() {
    let request = request(vec![
        Item::Message(Message::user("hello")),
        result("call-9", "orphan"),
    ]);
    assert_eq!(field(encode_err(&request)), "tool_result.id");
}

#[test]
fn a_failed_result_is_refused_rather_than_written_as_a_success() {
    let request = request(vec![
        Item::Message(Message::user("hello")),
        call("call-1", "weather", json!({})),
        failed_result("call-1", "boom"),
    ]);
    assert_eq!(field(encode_err(&request)), "tool_result.is_error");
}

#[test]
fn a_result_carrying_an_image_is_refused() {
    let request = request(vec![
        Item::Message(Message::user("hello")),
        call("call-1", "weather", json!({})),
        tool_result("call-1", vec![inline_image("image/png", b"png")], false),
    ]);
    assert_eq!(field(encode_err(&request)), "tool_result.content");
}

#[test]
fn a_remote_image_is_refused_because_this_dialect_has_no_url_field() {
    let request = one(message(
        Role::User,
        vec![remote_image("https://example.test/cat.png")],
    ));
    assert_eq!(field(encode_err(&request)), "content_part.image.url");
}

#[test]
fn an_inline_image_is_carried_as_inline_data() {
    let request = one(message(
        Role::User,
        vec![
            ContentPart::text("what is this?"),
            inline_image("image/png", b"png"),
        ],
    ));
    let document = encoded(&request);
    assert_eq!(
        document["contents"][0]["parts"][1],
        json!({"inlineData": {"mimeType": "image/png", "data": "cG5n"}})
    );
}

#[test]
fn a_file_part_is_refused_as_an_undeclared_capability() {
    let request = one(message(
        Role::User,
        vec![ContentPart::File {
            media_type: "application/pdf".to_owned(),
            data: b"%PDF".to_vec(),
            name: None,
        }],
    ));
    assert!(matches!(
        encode_err(&request),
        ProviderError::Unsupported {
            feature: nico_llm::Capability::Files,
            ..
        }
    ));
}

#[test]
fn a_reasoning_item_is_a_thought_part_with_its_signature() {
    let signed = request(vec![
        Item::Message(Message::user("think")),
        signed_reasoning("hmm", "sig-1"),
        assistant("answer"),
    ]);
    let document = encoded(&signed);
    assert_eq!(
        document["contents"][1]["parts"][0],
        json!({"text": "hmm", "thought": true, "thoughtSignature": "sig-1"})
    );
    // A reasoning item without a signature writes no signature member at all.
    let unsigned = encoded(&request(vec![
        Item::Message(Message::user("think")),
        reasoning("hmm"),
        assistant("answer"),
    ]));
    assert_eq!(
        unsigned["contents"][1]["parts"][0],
        json!({"text": "hmm", "thought": true})
    );
}

#[test]
fn a_refusal_never_quotes_the_value_it_refused() {
    let request = one(message(
        Role::User,
        vec![remote_image("https://example.test/a-secret-name.png")],
    ));
    let error = encode_err(&request);
    assert!(
        !format!("{error:?} {error}").contains("a-secret-name"),
        "the refusal names the field, not its value: {error}"
    );
}
