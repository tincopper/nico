//! Decoding a `/v1/responses` request: instructions, input items, tools and refusals.

use nico_llm::{
    protocol::CodecError,
    schema::{ContentPart, ImageSource, Item, Role, ToolChoice, ToolName},
};

use super::support::{decode, refusal, user};

#[test]
fn instructions_become_a_leading_system_message_and_a_string_input_is_one_user_message() {
    let body = r#"{
        "model": "gpt-5",
        "instructions": "be brief",
        "input": "hi"
    }"#;
    let request = decode(body).expect("the document is valid");
    assert_eq!(request.conversation().len(), 2);
    match &request.conversation()[0] {
        Item::Message(message) => {
            assert_eq!(message.role(), Role::System);
            assert_eq!(message.content(), [ContentPart::text("be brief")]);
        }
        other => panic!("expected a system message, got {other:?}"),
    }
    assert_eq!(request.conversation()[1], user("hi"));
}

#[test]
fn input_items_become_one_canonical_item_each_in_wire_order() {
    let body = r#"{
        "model": "gpt-5",
        "input": [
            {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "look"},
                {"type": "input_image", "image_url": "https://example.test/a.png"}
            ]},
            {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{\"q\":\"x\"}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "found"}
        ]
    }"#;
    let request = decode(body).expect("the document is valid");
    let conversation = request.conversation();
    assert_eq!(conversation.len(), 3);
    match &conversation[0] {
        Item::Message(message) => assert_eq!(
            message.content()[1],
            ContentPart::Image(ImageSource::Url("https://example.test/a.png".to_owned()))
        ),
        other => panic!("expected a message, got {other:?}"),
    }
    match &conversation[1] {
        Item::ToolCall(call) => {
            assert_eq!(call.id().as_str(), "call_1");
            assert_eq!(call.name().as_str(), "lookup");
        }
        other => panic!("expected a call, got {other:?}"),
    }
    match &conversation[2] {
        Item::ToolResult(result) => {
            assert_eq!(result.id().as_str(), "call_1");
            assert_eq!(result.content(), [ContentPart::text("found")]);
        }
        other => panic!("expected a result, got {other:?}"),
    }
}

#[test]
fn an_inline_image_is_decoded_from_a_data_url() {
    let body = r#"{
        "model": "gpt-5",
        "input": [{"type": "message", "role": "user", "content": [
            {"type": "input_text", "text": "look"},
            {"type": "input_image", "image_url": "data:image/png;base64,SGkh"}
        ]}]
    }"#;
    let request = decode(body).expect("the document is valid");
    match &request.conversation()[0] {
        Item::Message(message) => match &message.content()[1] {
            ContentPart::Image(ImageSource::Base64 { media_type, data }) => {
                assert_eq!(media_type, "image/png");
                assert_eq!(data, b"Hi!");
            }
            other => panic!("expected an inline image, got {other:?}"),
        },
        other => panic!("expected a message, got {other:?}"),
    }
}

#[test]
fn tools_are_flat_functions_and_each_choice_is_mapped() {
    let body = r#"{
        "model": "gpt-5",
        "input": "hi",
        "tools": [{"type": "function", "name": "lookup", "description": "find", "parameters": {"type": "object"}}],
        "tool_choice": {"type": "function", "name": "lookup"}
    }"#;
    let request = decode(body).expect("the document is valid");
    assert_eq!(request.tools().len(), 1);
    assert_eq!(request.tools()[0].name().as_str(), "lookup");
    assert_eq!(request.tools()[0].description(), Some("find"));
    assert_eq!(
        request.tool_choice(),
        &ToolChoice::Tool(ToolName::try_new("lookup").expect("valid"))
    );

    for (mode, expected) in [
        (r#""auto""#, ToolChoice::Auto),
        (r#""none""#, ToolChoice::None),
        (r#""required""#, ToolChoice::Required),
    ] {
        let body = body.replace(r#"{"type": "function", "name": "lookup"}"#, mode);
        assert_eq!(decode(&body).expect("valid").tool_choice(), &expected);
    }
}

#[test]
fn a_built_in_tool_is_refused_by_field() {
    let body = r#"{
        "model": "gpt-5",
        "input": "hi",
        "tools": [{"type": "web_search"}]
    }"#;
    match refusal(body) {
        CodecError::UnsupportedField { field, .. } => assert_eq!(field, "tools[0].type"),
        other => panic!("expected a field refusal, got {other:?}"),
    }
}

#[test]
fn the_structured_output_request_is_mapped() {
    let body = r#"{"model":"gpt-5","input":"hi","text":{"format":{"type":"json_object"}}}"#;
    let request = decode(body).expect("the document is valid");
    assert!(matches!(
        request.response_format(),
        nico_llm::ResponseFormat::JsonObject
    ));

    let body = r#"{
        "model": "gpt-5",
        "input": "hi",
        "text": {"format": {"type": "json_schema", "name": "answer", "schema": {"type": "object"}}}
    }"#;
    let request = decode(body).expect("the document is valid");
    match request.response_format() {
        nico_llm::ResponseFormat::JsonSchema(schema) => {
            assert_eq!(schema.name(), Some("answer"));
        }
        other => panic!("expected a schema format, got {other:?}"),
    }
}

#[test]
fn a_missing_input_is_a_malformed_document() {
    match refusal(r#"{"model":"gpt-5"}"#) {
        CodecError::InvalidRequest { reason, .. } => assert!(reason.contains("input"), "{reason}"),
        other => panic!("expected a malformed document, got {other:?}"),
    }
}

#[test]
fn a_stateful_or_retaining_request_is_refused_by_field() {
    for (field, body) in [
        ("store", r#"{"model":"gpt-5","input":"hi","store":true}"#),
        (
            "previous_response_id",
            r#"{"model":"gpt-5","input":"hi","previous_response_id":"resp_0"}"#,
        ),
        (
            "parallel_tool_calls",
            r#"{"model":"gpt-5","input":"hi","parallel_tool_calls":true}"#,
        ),
        (
            "service_tier",
            r#"{"model":"gpt-5","input":"hi","service_tier":"flex"}"#,
        ),
        (
            "reasoning",
            r#"{"model":"gpt-5","input":"hi","reasoning":{"effort":"high"}}"#,
        ),
    ] {
        match refusal(body) {
            CodecError::UnsupportedField { field: named, .. } => assert_eq!(named, field),
            other => panic!("expected `{field}` to be refused, got {other:?}"),
        }
    }
    // `store: false` is the gateway's own behaviour, so it is accepted.
    assert!(decode(r#"{"model":"gpt-5","input":"hi","store":false}"#).is_ok());
}

#[test]
fn a_reasoning_item_is_refused_as_a_missing_capability() {
    let body = r#"{
        "model": "gpt-5",
        "input": [
            {"type": "message", "role": "user", "content": "hi"},
            {"type": "reasoning", "id": "rs_1", "summary": []}
        ]
    }"#;
    assert!(matches!(refusal(body), CodecError::Unsupported { .. }));
}

#[test]
fn a_system_message_after_the_conversation_started_is_refused() {
    let body = r#"{
        "model": "gpt-5",
        "input": [
            {"type": "message", "role": "user", "content": "hi"},
            {"type": "message", "role": "system", "content": "late"}
        ]
    }"#;
    match refusal(body) {
        CodecError::UnsupportedField { field, .. } => assert_eq!(field, "input[1].role"),
        other => panic!("expected a field refusal, got {other:?}"),
    }
}

#[test]
fn labels_are_preserved_in_the_extensions() {
    let body = r#"{"model":"gpt-5","input":"hi","metadata":{"k":"v"},"user":"u-1"}"#;
    let request = decode(body).expect("the document is valid");
    assert!(request.extensions().get("metadata").is_some());
    assert!(request.extensions().get("user").is_some());
}

#[test]
fn an_unknown_item_type_is_a_malformed_document() {
    let body = r#"{"model":"gpt-5","input":[{"type":"hologram"}]}"#;
    assert!(matches!(refusal(body), CodecError::InvalidRequest { .. }));
}
