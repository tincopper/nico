//! Encoding the canonical request as a Chat Completions request document.

use nico_llm::{
    Capability, ContentPart, Extensions, Item, JsonSchema, ProviderError, RawJson, Reasoning,
    ResponseFormat, Role, Sampling, ToolChoice,
};
use serde_json::{Value, json};

use crate::support::{
    assistant, call, encode_err, encoded, encoded_on, inline_image, message, reasoning,
    remote_image, request, result, route_with, tool, user_request,
};

/// Asserts the document carries no member named `name`.
fn absent(body: &Value, name: &str) {
    assert!(
        body.get(name).is_none(),
        "`{name}` must be omitted, got {}",
        body[name]
    );
}

#[test]
fn the_physical_model_is_written_and_the_logical_one_is_not() {
    let route = route_with(None, &[("chat-small", "chat-small-2026-01-01")]);
    let body = encoded_on(&route, &user_request("hi"));
    assert_eq!(body["model"], "chat-small-2026-01-01");
    assert_eq!(body["messages"], json!([{"role": "user", "content": "hi"}]));
}

#[test]
fn a_model_the_route_does_not_map_keeps_its_name() {
    let body = encoded(&user_request("hi"));
    assert_eq!(body["model"], "chat-small");
}

#[test]
fn every_conversation_role_is_kept() {
    let body = encoded(&request(vec![
        message(Role::System, vec![ContentPart::text("be brief")]),
        message(Role::Developer, vec![ContentPart::text("cite sources")]),
        message(Role::User, vec![ContentPart::text("hello")]),
    ]));
    assert_eq!(
        body["messages"],
        json!([
            {"role": "system", "content": "be brief"},
            {"role": "developer", "content": "cite sources"},
            {"role": "user", "content": "hello"},
        ])
    );
}

#[test]
fn the_sampling_knobs_are_written() {
    let sampling = Sampling::default()
        .with_temperature(0.2)
        .expect("a valid temperature")
        .with_top_p(0.9)
        .expect("a valid top_p")
        .with_max_tokens(1024)
        .expect("a valid ceiling")
        .with_stop(vec!["END".to_owned()])
        .expect("valid stop sequences")
        .with_seed(7)
        .with_presence_penalty(0.5)
        .expect("a valid penalty")
        .with_frequency_penalty(-0.5)
        .expect("a valid penalty");
    let body = encoded(&user_request("hi").with_sampling(sampling));
    assert_eq!(body["temperature"], 0.2);
    assert_eq!(body["top_p"], 0.9);
    assert_eq!(body["max_tokens"], 1024);
    assert_eq!(body["stop"], json!(["END"]));
    assert_eq!(body["seed"], 7);
    assert_eq!(body["presence_penalty"], 0.5);
    assert_eq!(body["frequency_penalty"], -0.5);
}

#[test]
fn an_unset_sampling_knob_is_omitted() {
    let body = encoded(&user_request("hi"));
    for name in [
        "temperature",
        "top_p",
        "max_tokens",
        "stop",
        "seed",
        "presence_penalty",
        "frequency_penalty",
    ] {
        absent(&body, name);
    }
}

#[test]
fn the_declared_tools_are_written() {
    let mut declared = tool("lookup");
    declared = declared
        .with_description("Find a record")
        .expect("a valid description");
    let request = user_request("hi")
        .with_tools(vec![declared])
        .expect("declared tools");
    let body = encoded(&request);
    assert_eq!(
        body["tools"],
        json!([{
            "type": "function",
            "function": {
                "name": "lookup",
                "description": "Find a record",
                "parameters": {"type": "object"},
            }
        }])
    );
    assert_eq!(body["tool_choice"], "auto");
}

#[test]
fn every_tool_choice_mode_is_written() {
    let declared = vec![tool("lookup")];
    for (choice, expected) in [
        (ToolChoice::Auto, json!("auto")),
        (ToolChoice::None, json!("none")),
        (ToolChoice::Required, json!("required")),
    ] {
        let request = user_request("hi")
            .with_tools(declared.clone())
            .expect("declared tools")
            .with_tool_choice(choice)
            .expect("a declared choice");
        assert_eq!(encoded(&request)["tool_choice"], expected);
    }
}

#[test]
fn a_named_tool_choice_references_the_function() {
    let request = user_request("hi")
        .with_tools(vec![tool("lookup")])
        .expect("declared tools")
        .with_tool_choice(ToolChoice::Tool(
            nico_llm::ToolName::try_new("lookup").expect("a valid name"),
        ))
        .expect("a declared choice");
    assert_eq!(
        encoded(&request)["tool_choice"],
        json!({"type": "function", "function": {"name": "lookup"}})
    );
}

#[test]
fn a_request_without_tools_omits_the_choice() {
    let request = user_request("hi")
        .with_tool_choice(ToolChoice::Required)
        .expect("the canonical model accepts a mode with no tools");
    absent(&encoded(&request), "tool_choice");
}

#[test]
fn the_response_format_is_written_when_it_is_not_text() {
    absent(&encoded(&user_request("hi")), "response_format");

    let json_object = encoded(&user_request("hi").with_response_format(ResponseFormat::JsonObject));
    assert_eq!(
        json_object["response_format"],
        json!({"type": "json_object"})
    );

    let schema = JsonSchema::try_new(
        Some("answer".to_owned()),
        RawJson::new(json!({"type": "object"})),
    )
    .expect("a valid schema");
    let request = user_request("hi").with_response_format(ResponseFormat::JsonSchema(schema));
    assert_eq!(
        encoded(&request)["response_format"],
        json!({
            "type": "json_schema",
            "json_schema": {"name": "answer", "schema": {"type": "object"}},
        }),
        "`strict` is not written because the canonical model has no such field"
    );
}

#[test]
fn a_schema_without_a_name_gets_a_deterministic_one() {
    let schema =
        JsonSchema::try_new(None, RawJson::new(json!({"type": "object"}))).expect("a valid schema");
    let request = user_request("hi").with_response_format(ResponseFormat::JsonSchema(schema));
    assert_eq!(
        encoded(&request)["response_format"]["json_schema"]["name"],
        "response"
    );
}

#[test]
fn streaming_is_written_only_when_it_is_asked_for() {
    absent(&encoded(&user_request("hi")), "stream");
    assert_eq!(
        encoded(&user_request("hi").with_stream(true))["stream"],
        true
    );
}

#[test]
fn reasoning_text_and_calls_share_one_assistant_message() {
    let request = request(vec![
        reasoning("think"),
        assistant("answer"),
        call("call_1", "lookup", json!({"q": "x"})),
        call("call_2", "fetch", json!({})),
    ]);
    let body = encoded(&request);
    let messages = body["messages"]
        .as_array()
        .expect("the document holds a message list");
    assert_eq!(messages.len(), 1, "one wire message carries the whole turn");
    assert_eq!(messages[0]["role"], "assistant");
    assert_eq!(messages[0]["reasoning_content"], "think");
    assert_eq!(messages[0]["content"], "answer");
    assert_eq!(messages[0]["tool_calls"][0]["id"], "call_1");
    assert_eq!(messages[0]["tool_calls"][0]["function"]["name"], "lookup");
    assert_eq!(
        messages[0]["tool_calls"][0]["function"]["arguments"],
        json!("{\"q\":\"x\"}"),
        "the protocol carries arguments as a JSON string"
    );
    assert_eq!(messages[0]["tool_calls"][1]["id"], "call_2");
}

#[test]
fn a_turn_that_only_calls_tools_has_no_content() {
    let body = encoded(&request(vec![call("call_1", "lookup", json!({}))]));
    let message = &body["messages"][0];
    assert_eq!(message["role"], "assistant");
    assert_eq!(
        message["content"],
        Value::Null,
        "a tool-only turn has no visible content, which the protocol spells as null"
    );
}

#[test]
fn a_tool_result_is_a_tool_message() {
    let body = encoded(&request(vec![result("call_1", "42")]));
    assert_eq!(
        body["messages"],
        json!([{"role": "tool", "tool_call_id": "call_1", "content": "42"}])
    );
}

#[test]
fn the_message_order_survives_the_assistant_folding() {
    let body = encoded(&request(vec![
        message(Role::User, vec![ContentPart::text("ask")]),
        reasoning("think"),
        assistant("call it"),
        call("call_1", "lookup", json!({})),
        result("call_1", "42"),
        message(Role::User, vec![ContentPart::text("thanks")]),
    ]));
    let roles: Vec<&str> = body["messages"]
        .as_array()
        .expect("a message list")
        .iter()
        .map(|message| message["role"].as_str().expect("a role"))
        .collect();
    assert_eq!(roles, ["user", "assistant", "tool", "user"]);
}

#[test]
fn an_inline_image_becomes_a_data_url() {
    let body = encoded(&request(vec![message(
        Role::User,
        vec![ContentPart::text("look"), inline_image("image/png", b"f")],
    )]));
    assert_eq!(
        body["messages"][0]["content"],
        json!([
            {"type": "text", "text": "look"},
            {
                "type": "image_url",
                "image_url": {"url": "data:image/png;base64,Zg=="},
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
        body["messages"][0]["content"][0]["image_url"]["url"],
        "https://example.test/image.png"
    );
}

#[test]
fn an_image_outside_a_user_turn_is_refused() {
    // The protocol carries content per role: an image belongs to a user turn. The refusal names the
    // canonical field it could not carry, and answers 422 rather than blaming the gateway.
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
                ProviderError::UnsupportedField { field, .. } if field == "message.content"
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
            ProviderError::Unsupported {
                feature: Capability::Files,
                ..
            }
        ),
        "the adapter must not claim a file it cannot send"
    );
}

#[test]
fn a_signed_reasoning_item_is_refused() {
    let signed =
        Reasoning::try_new("think", Some("signature".to_owned())).expect("valid signed reasoning");
    let request = request(vec![Item::Reasoning(signed)]);
    assert!(matches!(
        encode_err(&request),
        ProviderError::UnsupportedField { field, .. } if field == "reasoning.signature"
    ));
}

#[test]
fn a_failed_tool_result_is_refused() {
    let failed = nico_llm::ToolResult::try_new(
        nico_llm::ToolCallId::try_new("call_1").expect("a valid id"),
        vec![ContentPart::text("it broke")],
    )
    .expect("a valid result")
    .with_error(true);
    let request = request(vec![Item::ToolResult(failed)]);
    assert!(matches!(
        encode_err(&request),
        ProviderError::UnsupportedField { field, .. } if field == "tool_result.is_error"
    ));
}

#[test]
fn a_tool_result_with_an_image_is_refused() {
    let answered = nico_llm::ToolResult::try_new(
        nico_llm::ToolCallId::try_new("call_1").expect("a valid id"),
        vec![remote_image("https://example.test/image.png")],
    )
    .expect("a valid result");
    let request = request(vec![Item::ToolResult(answered)]);
    assert!(matches!(
        encode_err(&request),
        ProviderError::UnsupportedField { field, .. } if field == "tool_result.content"
    ));
}

#[test]
fn an_unmodelled_field_is_written_back() {
    let mut extensions = Extensions::new();
    extensions
        .try_insert("user", RawJson::new(json!("tenant-1")))
        .expect("a valid key");
    let request = user_request("hi").with_extensions(extensions);
    assert_eq!(encoded(&request)["user"], "tenant-1");
}

#[test]
fn an_extension_that_shadows_a_modelled_field_is_refused() {
    let mut extensions = Extensions::new();
    extensions
        .try_insert("temperature", RawJson::new(json!(0.5)))
        .expect("the canonical model does not model a wire `temperature`");
    let request = user_request("hi").with_extensions(extensions);
    assert!(matches!(
        encode_err(&request),
        ProviderError::InvalidRequest { .. }
    ));
}
