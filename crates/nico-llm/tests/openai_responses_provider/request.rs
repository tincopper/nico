//! Encoding the canonical request as a Responses request document.

use nico_llm::{
    ContentPart, Extensions, Item, JsonSchema, RawJson, ResponseFormat, Role, Sampling, ToolChoice,
    ToolName,
};
use serde_json::{Value, json};

use crate::support::{encoded, encoded_on, message, request, route_mapping, tool, user_request};

/// Asserts the document carries no member named `name`.
fn absent(body: &Value, name: &str) {
    assert!(
        body.get(name).is_none(),
        "`{name}` must be omitted, got {}",
        body[name]
    );
}

/// Returns the `input` item at `index`.
fn item(body: &Value, index: usize) -> &Value {
    &body["input"][index]
}

mod items;

#[test]
fn the_physical_model_is_written_and_the_logical_one_is_not() {
    let route = route_mapping(&[("gpt-small", "gpt-small-2026-01-01")]);
    let body = encoded_on(&route, &user_request("hi"));
    assert_eq!(body["model"], "gpt-small-2026-01-01");
    assert_eq!(
        item(&body, 0),
        &json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "hi"}],
        })
    );
}

#[test]
fn a_model_the_route_does_not_map_keeps_its_name() {
    assert_eq!(encoded(&user_request("hi"))["model"], "gpt-small");
}

#[test]
fn the_leading_instruction_turns_merge_into_one_field() {
    let body = encoded(&request(vec![
        message(Role::System, vec![ContentPart::text("be brief")]),
        message(Role::Developer, vec![ContentPart::text("cite sources")]),
        message(Role::User, vec![ContentPart::text("hello")]),
    ]));
    assert_eq!(
        body["instructions"], "be brief\n\ncite sources",
        "separate turns stay separate paragraphs, in order"
    );
    assert_eq!(
        body["input"],
        json!([{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "hello"}],
        }]),
        "an instruction turn is not an input item"
    );
}

#[test]
fn an_instruction_turn_after_the_conversation_started_is_refused() {
    // The dialect carries instructions in one top-level string; a later instruction has no position,
    // and hoisting it would change what the model reads before the conversation.
    let request = request(vec![
        Item::Message(nico_llm::Message::user("hello")),
        message(Role::System, vec![ContentPart::text("late instructions")]),
    ]);
    assert!(
        matches!(
            crate::support::encode_err(&request),
            nico_llm::ProviderError::UnsupportedField { field, .. }
                if field == "conversation[].role"
        ),
        "a late instruction names the position it cannot take"
    );
}

#[test]
fn a_conversation_of_instructions_only_is_refused() {
    let request = request(vec![message(
        Role::System,
        vec![ContentPart::text("be brief")],
    )]);
    assert!(matches!(
        crate::support::encode_err(&request),
        nico_llm::ProviderError::UnsupportedField { field, .. } if field == "conversation"
    ));
}

#[test]
fn the_declared_tools_use_the_flat_function_shape() {
    let mut declared = tool("lookup");
    declared = declared
        .with_description("Find a record")
        .expect("a valid description");
    let request = user_request("hi")
        .with_tools(vec![declared])
        .expect("declared tools");
    assert_eq!(
        encoded(&request)["tools"],
        json!([{
            "type": "function",
            "name": "lookup",
            "description": "Find a record",
            "parameters": {"type": "object"},
        }]),
        "this dialect declares the function flat; chat's nested `function` object is a different wire"
    );
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
            ToolName::try_new("lookup").expect("a valid name"),
        ))
        .expect("a declared choice");
    assert_eq!(
        encoded(&request)["tool_choice"],
        json!({"type": "function", "name": "lookup"})
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
fn the_sampling_knobs_are_written() {
    let sampling = Sampling::default()
        .with_temperature(0.2)
        .expect("a valid temperature")
        .with_top_p(0.9)
        .expect("a valid top_p")
        .with_max_tokens(64)
        .expect("a valid ceiling");
    let body = encoded(&user_request("hi").with_sampling(sampling));
    assert_eq!(body["temperature"], 0.2);
    assert_eq!(body["top_p"], 0.9);
    assert_eq!(
        body["max_output_tokens"], 64,
        "the canonical ceiling is this dialect's `max_output_tokens`"
    );
}

#[test]
fn an_unset_sampling_knob_is_omitted() {
    let body = encoded(&user_request("hi"));
    for name in ["temperature", "top_p", "max_output_tokens", "stop"] {
        absent(&body, name);
    }
}

#[test]
fn the_sampling_knobs_this_dialect_lacks_are_refused() {
    let cases = [
        (
            Sampling::default()
                .with_stop(vec!["END".to_owned()])
                .expect("valid stop sequences"),
            "sampling.stop",
        ),
        (Sampling::default().with_seed(7), "sampling.seed"),
        (
            Sampling::default()
                .with_presence_penalty(0.5)
                .expect("a valid penalty"),
            "sampling.presence_penalty",
        ),
        (
            Sampling::default()
                .with_frequency_penalty(-0.5)
                .expect("a valid penalty"),
            "sampling.frequency_penalty",
        ),
    ];
    for (sampling, field) in cases {
        let error = crate::support::encode_err(&user_request("hi").with_sampling(sampling));
        assert!(
            matches!(&error, nico_llm::ProviderError::UnsupportedField { field: named, .. } if *named == field),
            "the refusal names `{field}`, got {error:?}"
        );
    }
}

#[test]
fn the_response_format_is_written_as_text_format() {
    let body = encoded(&user_request("hi"));
    absent(&body, "text");

    assert_eq!(
        encoded(&user_request("hi").with_response_format(ResponseFormat::JsonObject))["text"],
        json!({"format": {"type": "json_object"}})
    );

    let schema = JsonSchema::try_new(
        Some("answer".to_owned()),
        RawJson::new(json!({"type": "object"})),
    )
    .expect("a valid schema");
    let body =
        encoded(&user_request("hi").with_response_format(ResponseFormat::JsonSchema(schema)));
    assert_eq!(
        body["text"],
        json!({"format": {"type": "json_schema", "name": "answer", "schema": {"type": "object"}}}),
        "`strict` is not written: the canonical model has no such field"
    );
}

#[test]
fn a_schema_without_a_name_gets_the_documented_default() {
    let schema =
        JsonSchema::try_new(None, RawJson::new(json!({"type": "object"}))).expect("a valid schema");
    let body =
        encoded(&user_request("hi").with_response_format(ResponseFormat::JsonSchema(schema)));
    assert_eq!(body["text"]["format"]["name"], "response");
}

#[test]
fn the_gateway_never_asks_the_upstream_to_store_the_answer() {
    // The vendor default is `store: true`. The gateway keeps no response to continue from, so the
    // field is written on every request rather than left to the default.
    let body = encoded(&user_request("hi"));
    assert_eq!(body["store"], false);
    assert_eq!(
        encoded(&user_request("hi").with_stream(true))["store"],
        false,
        "a streamed answer is not stored either"
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
fn an_unmodelled_field_is_written_back() {
    let mut extensions = Extensions::new();
    extensions
        .try_insert("metadata", RawJson::new(json!({"tenant": "t1"})))
        .expect("a valid key");
    let request = user_request("hi").with_extensions(extensions);
    assert_eq!(encoded(&request)["metadata"], json!({"tenant": "t1"}));
}

#[test]
fn an_extension_that_shadows_a_modelled_field_is_refused() {
    let mut extensions = Extensions::new();
    extensions
        .try_insert("store", RawJson::new(json!(true)))
        .expect("the canonical model does not model a wire `store`");
    let request = user_request("hi").with_extensions(extensions);
    assert!(
        matches!(
            crate::support::encode_err(&request),
            nico_llm::ProviderError::InvalidRequest { .. }
        ),
        "writing the relayed value next to the modelled one would leave two answers"
    );
}

#[test]
fn an_extension_naming_a_refused_request_member_is_refused() {
    // The canonical request cannot carry these, so a relayed key is the only way they can arrive —
    // and writing one upstream would ask for something the caller never expressed.
    for (key, value) in [
        ("previous_response_id", json!("resp_1")),
        ("parallel_tool_calls", json!(false)),
        ("truncation", json!("auto")),
        ("service_tier", json!("flex")),
        ("include", json!(["reasoning.encrypted_content"])),
        ("background", json!(true)),
        ("reasoning", json!({"effort": "low"})),
        ("prompt", json!({"id": "pmpt_1"})),
    ] {
        let mut extensions = Extensions::new();
        extensions
            .try_insert(key, RawJson::new(value))
            .expect("a key");
        let request = user_request("hi").with_extensions(extensions);
        let error = crate::support::encode_err(&request);
        assert!(
            matches!(&error, nico_llm::ProviderError::UnsupportedField { field, .. } if field == key),
            "`{key}` must be refused by name, got {error:?}"
        );
    }
}
