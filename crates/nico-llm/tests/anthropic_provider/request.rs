//! Encoding the canonical request as a Messages request document: the envelope and its fields.
//!
//! The conversation folding (runs, tool results, content blocks) is asserted in [`conversation`].

use nico_llm::{
    Capability, ContentPart, Extensions, Item, RawJson, Reasoning, ResponseFormat, Role, Sampling,
    ToolChoice, ToolName,
};
use serde_json::{Value, json};

use crate::support::{
    BASE_URL, CEILING, encode_err, encoded, encoded_on, message, reasoning, request, route_mapping,
    route_with, tool, user_request,
};

mod conversation;

/// Asserts the document carries no member named `name`.
fn absent(body: &Value, name: &str) {
    assert!(
        body.get(name).is_none(),
        "`{name}` must be omitted, got {}",
        body[name]
    );
}

/// Returns the wire message at `index`.
fn wire_message(body: &Value, index: usize) -> &Value {
    &body["messages"][index]
}

#[test]
fn the_physical_model_is_written_and_the_logical_one_is_not() {
    let route = route_mapping(&[("claude-small", "claude-small-2026-01-01")]);
    let body = encoded_on(&route, &user_request("hi"));
    assert_eq!(body["model"], "claude-small-2026-01-01");
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(
        body["messages"][0]["content"],
        json!([{"type": "text", "text": "hi"}])
    );
}

#[test]
fn a_model_the_route_does_not_map_keeps_its_name() {
    assert_eq!(encoded(&user_request("hi"))["model"], "claude-small");
}

#[test]
fn the_leading_system_turns_become_the_top_level_field() {
    let body = encoded(&request(vec![
        message(Role::System, vec![ContentPart::text("be brief")]),
        message(Role::Developer, vec![ContentPart::text("cite sources")]),
        message(Role::User, vec![ContentPart::text("hello")]),
    ]));
    assert_eq!(
        body["system"],
        json!([
            {"type": "text", "text": "be brief"},
            {"type": "text", "text": "cite sources"},
        ]),
        "both instruction roles merge into one top-level field, in order"
    );
    assert_eq!(
        body["messages"],
        json!([{"role": "user", "content": [{"type": "text", "text": "hello"}]}])
    );
}

#[test]
fn a_system_turn_after_the_conversation_started_is_refused() {
    // The protocol carries instructions in one top-level field; a later instruction has no position,
    // and hoisting it would change what the model reads before the conversation.
    let request = request(vec![
        Item::Message(nico_llm::Message::user("hello")),
        message(Role::System, vec![ContentPart::text("late instructions")]),
    ]);
    assert!(
        matches!(
            encode_err(&request),
            nico_llm::ProviderError::UnsupportedField { field, .. }
                if field == "conversation[].role"
        ),
        "a late system turn names the position it cannot take"
    );
}

#[test]
fn the_output_ceiling_is_always_written() {
    // `/v1/messages` requires `max_tokens`; the route answers for a request that names none.
    let body = encoded(&user_request("hi"));
    assert_eq!(body["max_tokens"], CEILING);
}

#[test]
fn the_caller_ceiling_wins_over_the_route_default() {
    let sampling = Sampling::default()
        .with_max_tokens(16)
        .expect("a valid ceiling");
    let body = encoded(&user_request("hi").with_sampling(sampling));
    assert_eq!(body["max_tokens"], 16);
}

#[test]
fn the_sampling_knobs_are_written() {
    let sampling = Sampling::default()
        .with_temperature(0.2)
        .expect("a valid temperature")
        .with_top_p(0.9)
        .expect("a valid top_p")
        .with_stop(vec!["END".to_owned()])
        .expect("valid stop sequences");
    let body = encoded(&user_request("hi").with_sampling(sampling));
    assert_eq!(body["temperature"], 0.2);
    assert_eq!(body["top_p"], 0.9);
    assert_eq!(body["stop_sequences"], json!(["END"]));
}

#[test]
fn an_unset_sampling_knob_is_omitted() {
    let body = encoded(&user_request("hi"));
    for name in ["temperature", "top_p", "stop_sequences", "seed"] {
        absent(&body, name);
    }
}

#[test]
fn a_temperature_above_the_protocol_range_is_refused_rather_than_clamped() {
    // The canonical model takes the widest range any target allows; this protocol's own range is
    // narrower. Writing a smaller value would change the output the caller asked for.
    for value in [1.5, 2.0] {
        let sampling = Sampling::default()
            .with_temperature(value)
            .expect("the canonical model allows up to 2.0");
        let error = encode_err(&user_request("hi").with_sampling(sampling));
        assert!(
            matches!(
                &error,
                nico_llm::ProviderError::UnsupportedField { field, .. } if field == "temperature"
            ),
            "{value} must be refused, got {error:?}"
        );
        assert!(
            !format!("{error:?} {error}").contains(&value.to_string()),
            "a refusal names the field and not its value: {error}"
        );
    }
}

#[test]
fn the_sampling_knobs_this_protocol_lacks_are_refused() {
    let cases = [
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
        let error = encode_err(&user_request("hi").with_sampling(sampling));
        assert!(
            matches!(&error, nico_llm::ProviderError::UnsupportedField { field: named, .. } if *named == field),
            "the refusal names `{field}`, got {error:?}"
        );
    }
}

#[test]
fn the_declared_tools_are_written_with_their_input_schema() {
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
            "name": "lookup",
            "description": "Find a record",
            "input_schema": {"type": "object"},
        }]),
        "this protocol inlines the schema; it has no nested `function` object"
    );
}

#[test]
fn every_tool_choice_mode_is_written() {
    let declared = vec![tool("lookup")];
    for (choice, expected) in [
        (ToolChoice::Auto, json!({"type": "auto"})),
        (ToolChoice::None, json!({"type": "none"})),
        (ToolChoice::Required, json!({"type": "any"})),
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
fn a_named_tool_choice_references_the_tool() {
    let request = user_request("hi")
        .with_tools(vec![tool("lookup")])
        .expect("declared tools")
        .with_tool_choice(ToolChoice::Tool(
            ToolName::try_new("lookup").expect("a valid name"),
        ))
        .expect("a declared choice");
    assert_eq!(
        encoded(&request)["tool_choice"],
        json!({"type": "tool", "name": "lookup"})
    );
}

#[test]
fn a_request_without_tools_omits_the_choice() {
    // The protocol refuses `any` on a request with no tools, and the canonical model allows the mode.
    let request = user_request("hi")
        .with_tool_choice(ToolChoice::Required)
        .expect("the canonical model accepts a mode with no tools");
    absent(&encoded(&request), "tool_choice");
}

#[test]
fn a_reasoning_item_is_refused() {
    // Writing a `thinking` block requires enabling thinking for the request, and the canonical model
    // has no budget knob; a block without it is refused upstream, so the item is refused here.
    let signed =
        Reasoning::try_new("think", Some("signature".to_owned())).expect("valid reasoning");
    for item in [reasoning("think"), Item::Reasoning(signed)] {
        let request = request(vec![item]);
        assert!(
            matches!(
                encode_err(&request),
                nico_llm::ProviderError::UnsupportedField { field, .. }
                    if field == "conversation[].reasoning"
            ),
            "a reasoning item has no field this adapter can write"
        );
    }
}

#[test]
fn a_structured_response_format_is_refused() {
    // This protocol has no JSON mode, so neither side declares the capability.
    for format in [
        ResponseFormat::JsonObject,
        ResponseFormat::JsonSchema(
            nico_llm::JsonSchema::try_new(None, RawJson::new(json!({"type": "object"})))
                .expect("a valid schema"),
        ),
    ] {
        let request = user_request("hi").with_response_format(format);
        assert!(
            matches!(
                encode_err(&request),
                nico_llm::ProviderError::Unsupported {
                    feature: Capability::ResponseFormat,
                    ..
                }
            ),
            "the adapter must not accept a format it cannot request"
        );
    }
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
        .try_insert("stop_sequences", RawJson::new(json!(["X"])))
        .expect("the canonical model does not model a wire `stop_sequences`");
    let request = user_request("hi").with_extensions(extensions);
    assert!(matches!(
        encode_err(&request),
        nico_llm::ProviderError::InvalidRequest { .. }
    ));
}

#[test]
fn the_endpoint_and_the_encoder_agree_about_the_ceiling() {
    // A route without a ceiling cannot address an endpoint; if an encoder is nevertheless reached,
    // it reports the same route fact instead of writing a body with no `max_tokens`.
    let route = route_with(BASE_URL, None, None);
    let error =
        nico_llm::Provider::encode(&crate::support::provider(), &route, &user_request("hi"))
            .expect_err("the encoder cannot invent a ceiling");
    assert!(
        matches!(
            error,
            nico_llm::ProviderError::InvalidEndpoint {
                source: nico_llm::EndpointError::RouteFact { .. },
                ..
            }
        ),
        "{error:?}"
    );
}
