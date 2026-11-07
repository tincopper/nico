//! Encoding a canonical request: the instruction, tools, sampling and the output shape.
//!
//! The conversation's own folding has its own file ([`conversation`]): it is a state machine over the
//! canonical items, while this file is the document around it.

use std::num::NonZeroU32;

use nico_llm::{
    ContentPart, Extensions, Item, Message, ProviderError, RawJson, ResponseFormat, Role, Sampling,
    ToolChoice, ToolName,
};
use serde_json::json;

use crate::support::{
    BASE_URL, LOGICAL, PHYSICAL, encode_err, encoded, encoded_on, inline_image, message, one,
    request, route, tool, user_request,
};

mod conversation;

/// Returns the refusal an encoded request must produce, as its canonical field name.
fn field(error: ProviderError) -> String {
    match error {
        ProviderError::UnsupportedField { field, .. } => field.to_string(),
        // A different refusal still fails the caller's assertion on the field name, and its variant is
        // the only diagnostic this helper reports.
        other => format!("{other:?}"),
    }
}

#[test]
fn leading_instructions_become_the_system_instruction_and_are_merged() {
    let document = encoded(&request(vec![
        message(Role::System, vec![ContentPart::text("be brief")]),
        message(Role::Developer, vec![ContentPart::text("be kind")]),
        Item::Message(Message::user("hello")),
    ]));
    assert_eq!(
        document["systemInstruction"],
        json!({"parts": [{"text": "be brief"}, {"text": "be kind"}]}),
        "this dialect's instruction carries no role member"
    );
    assert_eq!(
        document["contents"],
        json!([{"role": "user", "parts": [{"text": "hello"}]}])
    );
    assert_eq!(
        document["systemInstruction"].get("role"),
        None,
        "writing a role would be a member the peer has no meaning for"
    );
}

#[test]
fn an_instruction_may_not_carry_an_image() {
    let request = request(vec![
        message(
            Role::System,
            vec![ContentPart::text("look"), inline_image("image/png", b"png")],
        ),
        Item::Message(Message::user("hello")),
    ]);
    assert_eq!(field(encode_err(&request)), "system_instruction.parts");
}

#[test]
fn declared_tools_are_one_declaration_group() {
    let request = one(Item::Message(Message::user("hi")))
        .with_tools(vec![
            tool("weather"),
            tool("clock")
                .with_description("tell time")
                .expect("a description"),
        ])
        .expect("tools are valid");
    let document = encoded(&request);
    assert_eq!(
        document["tools"],
        json!([{
            "functionDeclarations": [
                {"name": "weather", "parameters": {"type": "object"}},
                {"name": "clock", "description": "tell time", "parameters": {"type": "object"}}
            ]
        }])
    );
    assert_eq!(
        document["toolConfig"],
        json!({"functionCallingConfig": {"mode": "AUTO"}})
    );
}

#[test]
fn a_request_without_tools_writes_neither_tools_nor_a_tool_config() {
    let document = encoded(&user_request("hi"));
    assert_eq!(document.get("tools"), None);
    assert_eq!(
        document.get("toolConfig"),
        None,
        "a calling mode with no declarations asks for a call the model cannot make"
    );
}

#[test]
fn every_tool_choice_has_a_mode() {
    let cases = [
        (ToolChoice::Auto, json!({"mode": "AUTO"})),
        (ToolChoice::None, json!({"mode": "NONE"})),
        (ToolChoice::Required, json!({"mode": "ANY"})),
        (
            ToolChoice::Tool(ToolName::try_new("weather").expect("a name")),
            json!({"mode": "ANY", "allowedFunctionNames": ["weather"]}),
        ),
    ];
    for (choice, expected) in cases {
        let request = one(Item::Message(Message::user("hi")))
            .with_tools(vec![tool("weather")])
            .expect("tools are valid")
            .with_tool_choice(choice)
            .expect("the choice is valid");
        assert_eq!(
            encoded(&request)["toolConfig"]["functionCallingConfig"],
            expected
        );
    }
}

#[test]
fn every_sampling_knob_has_a_generation_config_member() {
    let sampling = Sampling::try_new(Some(0.5), Some(0.9), Some(64))
        .expect("valid knobs")
        .with_stop(vec!["END".to_owned()])
        .expect("a valid stop")
        .with_seed(7)
        .with_presence_penalty(0.1)
        .expect("a valid penalty")
        .with_frequency_penalty(0.2)
        .expect("a valid penalty");
    let request = one(Item::Message(Message::user("hi"))).with_sampling(sampling);
    assert_eq!(
        encoded(&request)["generationConfig"],
        json!({
            "temperature": 0.5,
            "topP": 0.9,
            "maxOutputTokens": 64,
            "stopSequences": ["END"],
            "presencePenalty": 0.1,
            "frequencyPenalty": 0.2,
            "seed": 7
        })
    );
}

#[test]
fn a_request_with_no_knobs_and_text_output_writes_no_generation_config() {
    let document = encoded(&user_request("hi"));
    assert_eq!(
        document.get("generationConfig"),
        None,
        "an empty object would say the caller configured nothing"
    );
}

#[test]
fn the_route_ceiling_is_not_applied_because_this_dialect_allows_omission() {
    // `providers[].max_output_tokens` is the default only for a dialect that requires a ceiling.
    // Writing it here would cap an answer the caller never capped.
    let ceiling = route(BASE_URL).with_max_output_tokens(NonZeroU32::new(1024).expect("non-zero"));
    let document = encoded_on(&ceiling, &user_request("hi"));
    assert_eq!(document.get("generationConfig"), None);
}

#[test]
fn the_callers_ceiling_is_written_verbatim() {
    let sampling = Sampling::try_new(None, None, Some(32)).expect("a valid ceiling");
    let request = one(Item::Message(Message::user("hi"))).with_sampling(sampling);
    assert_eq!(encoded(&request)["generationConfig"]["maxOutputTokens"], 32);
}

#[test]
fn a_structured_response_format_is_a_mime_type_and_a_schema() {
    let schema = nico_llm::JsonSchema::try_new(
        Some("answer".to_owned()),
        RawJson::new(json!({"type": "object", "properties": {"a": {"type": "string"}}})),
    )
    .expect("a valid schema");
    for (format, expected) in [
        (ResponseFormat::Text, json!({})),
        (
            ResponseFormat::JsonObject,
            json!({"responseMimeType": "application/json"}),
        ),
        (
            ResponseFormat::JsonSchema(schema),
            json!({
                "responseMimeType": "application/json",
                "responseSchema": {"type": "object", "properties": {"a": {"type": "string"}}}
            }),
        ),
    ] {
        let request = one(Item::Message(Message::user("hi"))).with_response_format(format);
        let document = encoded(&request);
        let config = document
            .get("generationConfig")
            .cloned()
            .unwrap_or_else(|| json!({}));
        assert_eq!(config, expected);
    }
}

#[test]
fn an_unmodelled_top_level_field_is_written_back() {
    let mut extensions = Extensions::new();
    extensions
        .try_insert("labels", RawJson::new(json!({"team": "search"})))
        .expect("a free key");
    let request = one(Item::Message(Message::user("hi"))).with_extensions(extensions);
    assert_eq!(
        encoded(&request)["labels"],
        json!({"team": "search"}),
        "a field the canonical model does not name is preserved, not dropped"
    );
}

#[test]
fn an_extension_key_that_shadows_a_member_is_refused() {
    for key in [
        "contents",
        "systemInstruction",
        "tools",
        "toolConfig",
        "generationConfig",
    ] {
        let mut extensions = Extensions::new();
        extensions
            .try_insert(key, RawJson::new(json!({})))
            .expect("a free key");
        let request = one(Item::Message(Message::user("hi"))).with_extensions(extensions);
        assert!(
            matches!(encode_err(&request), ProviderError::InvalidRequest { .. }),
            "`{key}` would be written next to a modelled member"
        );
    }
}

#[test]
fn the_document_names_neither_the_model_nor_the_stream_flag() {
    // Both are publication-time facts of this adapter: the model is in the path, and the operation is
    // always the streaming one.
    let document = encoded(&one(Item::Message(Message::user("hi"))).with_stream(true));
    assert_eq!(document.get("model"), None);
    assert_eq!(document.get("stream"), None);
}

#[test]
fn the_same_logical_name_encodes_on_a_route_that_pins_another_model() {
    // The body carries no model at all, so it is the exchange — not the encoder — that refuses a
    // request whose resolved model is not the one the endpoint pinned.
    let route = route(BASE_URL)
        .with_model_map(std::collections::BTreeMap::from([(
            LOGICAL.to_owned(),
            "gemini-2.0-pro".to_owned(),
        )]))
        .expect("a valid map");
    let document = encoded_on(&route, &user_request("hi"));
    assert_eq!(document["contents"][0]["role"], "user");
    assert!(
        !document.to_string().contains(PHYSICAL),
        "the body names no model for either value"
    );
}

#[test]
fn a_system_message_with_a_file_names_the_capability() {
    let request = request(vec![
        message(
            Role::System,
            vec![ContentPart::File {
                media_type: "text/plain".to_owned(),
                data: b"x".to_vec(),
                name: None,
            }],
        ),
        Item::Message(Message::user("hi")),
    ]);
    assert!(matches!(
        encode_err(&request),
        ProviderError::Unsupported {
            feature: nico_llm::Capability::Files,
            ..
        }
    ));
}
