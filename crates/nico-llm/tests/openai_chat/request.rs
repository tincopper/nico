//! What one request document becomes: items, tools, sampling, formats and the fields that stay out.

use nico_llm::{
    ClientCodec, CodecError, ContentPart, Item, ResponseFormat, Role, Sampling, ToolChoice,
    protocol::openai_chat::PROTOCOL,
};
use serde_json::json;

use super::support::{decode, decode_bytes, decode_err, user_body};

/// A request body with extra top-level members.
fn with_extra(extra: &str) -> String {
    format!(r#"{{"model":"m","messages":[{{"role":"user","content":"hi"}}],{extra}}}"#)
}

#[test]
fn a_plain_user_message_decodes_to_one_item() {
    let request = decode(&user_body("hello"));
    assert_eq!(request.model().as_str(), "chat-small");
    assert!(!request.stream());
    assert!(request.tools().is_empty());
    assert!(matches!(request.tool_choice(), ToolChoice::Auto));
    assert_eq!(request.sampling(), &Sampling::default());
    assert_eq!(request.response_format(), &ResponseFormat::Text);
    assert!(request.extensions().is_empty());
    assert_eq!(request.conversation().len(), 1);
    match &request.conversation()[0] {
        Item::Message(message) => {
            assert_eq!(message.role(), Role::User);
            assert_eq!(message.content(), [ContentPart::text("hello")]);
        }
        other => panic!("expected a message, got {other:?}"),
    }
}

#[test]
fn the_system_and_developer_roles_keep_their_meaning() {
    let request = decode(
        r#"{"model":"m","messages":[
            {"role":"system","content":"be brief"},
            {"role":"developer","content":"answer in JSON"},
            {"role":"user","content":"hi"}
        ]}"#,
    );
    let roles = request
        .conversation()
        .iter()
        .filter_map(|item| match item {
            Item::Message(message) => Some(message.role()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(roles, vec![Role::System, Role::Developer, Role::User]);
}

#[test]
fn an_assistant_turn_becomes_reasoning_text_and_calls_in_order() {
    let request = decode(
        r#"{"model":"m","messages":[
            {"role":"user","content":"weather?"},
            {"role":"assistant","reasoning_content":"I should call the tool","content":"Let me check.","tool_calls":[
                {"id":"call_1","type":"function","function":{"name":"weather","arguments":"{\"city\":\"Paris\"}"}}
            ]}
        ]}"#,
    );
    let items = request.conversation();
    assert_eq!(items.len(), 4);
    assert!(
        matches!(&items[1], Item::Reasoning(reasoning) if reasoning.text() == "I should call the tool"),
        "reasoning comes before the visible answer: {:?}",
        items[1]
    );
    assert!(
        matches!(&items[2], Item::Message(message) if message.role() == Role::Assistant),
        "the visible answer is an assistant message: {:?}",
        items[2]
    );
    match &items[3] {
        Item::ToolCall(call) => {
            assert_eq!(call.id().as_str(), "call_1");
            assert_eq!(call.name().as_str(), "weather");
            assert_eq!(call.arguments().as_value(), &json!({"city": "Paris"}));
        }
        other => panic!("expected a tool call, got {other:?}"),
    }
}

#[test]
fn an_assistant_message_with_only_tool_calls_has_no_message_item() {
    let request = decode(
        r#"{"model":"m","messages":[
            {"role":"user","content":"hi"},
            {"role":"assistant","content":null,"tool_calls":[
                {"id":"c1","type":"function","function":{"name":"f","arguments":"{}"}}
            ]}
        ]}"#,
    );
    assert_eq!(request.conversation().len(), 2);
    assert!(
        matches!(&request.conversation()[1], Item::ToolCall(call) if call.id().as_str() == "c1"),
        "an empty assistant message is not invented: {:?}",
        request.conversation()[1]
    );
}

#[test]
fn an_empty_reasoning_content_is_not_an_item() {
    let request = decode(
        r#"{"model":"m","messages":[
            {"role":"assistant","reasoning_content":"","content":"hi"}
        ]}"#,
    );
    assert_eq!(request.conversation().len(), 1);
    assert!(matches!(&request.conversation()[0], Item::Message(_)));
}

#[test]
fn tools_and_a_named_choice_decode() {
    let request = decode(
        r#"{"model":"m","messages":[{"role":"user","content":"hi"}],
            "tools":[{"type":"function","function":{
                "name":"weather",
                "description":"Look up the weather",
                "parameters":{"type":"object","properties":{"city":{"type":"string"}}}
            }}],
            "tool_choice":{"type":"function","function":{"name":"weather"}}}"#,
    );
    assert_eq!(request.tools().len(), 1);
    assert_eq!(request.tools()[0].name().as_str(), "weather");
    assert_eq!(
        request.tools()[0].description(),
        Some("Look up the weather")
    );
    assert!(request.tools()[0].parameters().is_object());
    assert!(
        matches!(request.tool_choice(), ToolChoice::Tool(name) if name.as_str() == "weather"),
        "the named choice must survive: {:?}",
        request.tool_choice()
    );
}

#[test]
fn a_tool_without_parameters_declares_an_empty_schema() {
    let request = decode(
        r#"{"model":"m","messages":[{"role":"user","content":"hi"}],
            "tools":[{"type":"function","function":{"name":"now"}}]}"#,
    );
    assert_eq!(request.tools()[0].parameters().as_value(), &json!({}));
}

#[test]
fn the_mode_tool_choices_decode() {
    for (spelling, expected) in [
        ("\"auto\"", ToolChoice::Auto),
        ("\"none\"", ToolChoice::None),
        ("\"required\"", ToolChoice::Required),
    ] {
        let request = decode(&with_extra(&format!(
            r#""tools":[{{"type":"function","function":{{"name":"f","parameters":{{}}}}}}],"tool_choice":{spelling}"#
        )));
        assert_eq!(request.tool_choice(), &expected, "choice {spelling}");
    }
}

#[test]
fn sampling_knobs_decode_into_the_canonical_value() {
    let request = decode(&with_extra(
        r#""temperature":0.2,"top_p":0.9,"max_completion_tokens":64,
            "stop":["\n\n","END"],"seed":7,"presence_penalty":0.1,"frequency_penalty":-0.2"#,
    ));
    let sampling = request.sampling();
    assert_eq!(sampling.temperature(), Some(0.2));
    assert_eq!(sampling.top_p(), Some(0.9));
    assert_eq!(sampling.max_tokens(), Some(64));
    assert_eq!(sampling.stop(), ["\n\n".to_owned(), "END".to_owned()]);
    assert_eq!(sampling.seed(), Some(7));
    assert_eq!(sampling.presence_penalty(), Some(0.1));
    assert_eq!(sampling.frequency_penalty(), Some(-0.2));
}

#[test]
fn a_single_stop_string_is_one_sequence() {
    let request = decode(&with_extra(r#""stop":"END""#));
    assert_eq!(request.sampling().stop(), ["END".to_owned()]);
}

#[test]
fn the_two_output_ceiling_spellings_must_agree() {
    let request = decode(&with_extra(r#""max_tokens":8,"max_completion_tokens":8"#));
    assert_eq!(request.sampling().max_tokens(), Some(8));
    assert!(
        matches!(
            decode_err(&with_extra(r#""max_tokens":8,"max_completion_tokens":9"#)),
            CodecError::InvalidRequest { .. }
        ),
        "picking one ceiling silently would change what the caller asked for"
    );
}

#[test]
fn a_structured_response_format_decodes() {
    let request = decode(&with_extra(
        r#""response_format":{"type":"json_schema","json_schema":{
            "name":"answer",
            "schema":{"type":"object","properties":{"value":{"type":"integer"}}}
        }}"#,
    ));
    match request.response_format() {
        ResponseFormat::JsonSchema(schema) => {
            assert_eq!(schema.name(), Some("answer"));
            assert_eq!(
                schema.schema().as_value(),
                &json!({"type":"object","properties":{"value":{"type":"integer"}}})
            );
        }
        other => panic!("expected a JSON schema, got {other:?}"),
    }
}

#[test]
fn the_other_response_formats_decode() {
    assert_eq!(
        decode(&with_extra(r#""response_format":{"type":"text"}"#)).response_format(),
        &ResponseFormat::Text
    );
    assert_eq!(
        decode(&with_extra(r#""response_format":{"type":"json_object"}"#)).response_format(),
        &ResponseFormat::JsonObject
    );
}

#[test]
fn an_unmodelled_top_level_field_is_preserved() {
    let request = decode(&with_extra(
        r#""metadata":{"trace":"abc"},"x_vendor":{"fast":true}"#,
    ));
    assert_eq!(
        request
            .extensions()
            .get("metadata")
            .map(|value| value.as_value()),
        Some(&json!({"trace": "abc"}))
    );
    let encoded = serde_json::to_value(&request).expect("the canonical request serializes");
    assert_eq!(
        encoded["x_vendor"],
        json!({"fast": true}),
        "a preserved field must be written back, not dropped"
    );
}

#[test]
fn a_known_field_the_canonical_model_cannot_carry_is_refused_by_name() {
    for (extra, field) in [
        (r#""n":2"#, "n"),
        (r#""audio":{"voice":"alloy","format":"mp3"}"#, "audio"),
        (r#""logprobs":true"#, "logprobs"),
        (r#""top_logprobs":2"#, "top_logprobs"),
        (r#""logit_bias":{"50256":-100}"#, "logit_bias"),
        (r#""modalities":["text","audio"]"#, "modalities"),
        (r#""parallel_tool_calls":false"#, "parallel_tool_calls"),
        (r#""functions":[]"#, "functions"),
        (r#""function_call":"none""#, "function_call"),
        (r#""reasoning_effort":"high""#, "reasoning_effort"),
        (
            r#""prediction":{"type":"content","content":"x"}"#,
            "prediction",
        ),
        (r#""web_search_options":{}"#, "web_search_options"),
        (r#""store":true"#, "store"),
        (r#""service_tier":"priority""#, "service_tier"),
    ] {
        assert_eq!(
            decode_err(&with_extra(extra)),
            CodecError::UnsupportedField {
                protocol: PROTOCOL,
                field: field.into()
            },
            "the field `{field}` must be refused, never dropped"
        );
    }
}

#[test]
fn a_refused_field_that_asks_for_nothing_is_accepted() {
    let request = decode(&with_extra(
        r#""n":1,"logprobs":false,"top_logprobs":0,"logit_bias":{},
            "modalities":["text"],"parallel_tool_calls":true,"store":false,
            "service_tier":"auto","stream_options":{"include_usage":true}"#,
    ));
    assert_eq!(request.model().as_str(), "m");
}

#[test]
fn a_named_tool_choice_must_be_declared() {
    assert!(
        matches!(
            decode_err(&with_extra(
                r#""tool_choice":{"type":"function","function":{"name":"absent"}}"#
            )),
            CodecError::InvalidRequest { .. }
        ),
        "a choice naming an undeclared tool breaks the canonical request"
    );
}

#[test]
fn a_request_needs_a_model_and_a_conversation() {
    assert!(matches!(
        decode_err(r#"{"messages":[{"role":"user","content":"hi"}]}"#),
        CodecError::InvalidRequest { .. }
    ));
    assert!(matches!(
        decode_err(r#"{"model":"m","messages":[]}"#),
        CodecError::InvalidRequest { .. }
    ));
}

#[test]
fn a_body_that_is_not_json_never_echoes_its_content() {
    let secret = "PROMPT-SECRET-1234";
    let broken = format!(r#"{{"model":"m","messages":[{{"role":"user","content":"{secret}"}}"#);
    match decode_err(&broken) {
        CodecError::InvalidRequest { reason, .. } => {
            assert!(
                !reason.contains(secret),
                "a parse diagnostic must not quote the prompt: {reason}"
            );
            assert!(
                reason.contains("line"),
                "the diagnostic must locate the failure: {reason}"
            );
        }
        other => panic!("expected an invalid request, got {other:?}"),
    }
}

#[test]
fn a_body_that_is_not_utf8_is_refused() {
    let error = super::support::codec()
        .decode_request(&super::support::facts(), b"{\"model\":\"\xff\"}")
        .expect_err("invalid UTF-8 is not a document");
    assert!(
        matches!(&error, CodecError::InvalidRequest { reason, .. } if reason.contains("UTF-8")),
        "expected a UTF-8 diagnostic, got {error:?}"
    );
    // An empty body is a document the parser rejects, not a panic.
    assert!(matches!(
        super::support::codec()
            .decode_request(&super::support::facts(), b"")
            .expect_err("an empty body is not a document"),
        CodecError::InvalidRequest { .. }
    ));
    assert_eq!(
        decode_bytes(user_body("hi").as_bytes())
            .conversation()
            .len(),
        1
    );
}

#[test]
fn a_stream_request_is_marked_as_one() {
    assert!(decode(&with_extra(r#""stream":true"#)).stream());
    assert!(!decode(&with_extra(r#""stream":false"#)).stream());
}

#[test]
fn a_response_format_this_gateway_cannot_carry_is_refused_by_field() {
    assert_eq!(
        decode_err(&with_extra(r#""response_format":{"type":"grammar"}"#)),
        CodecError::UnsupportedField {
            protocol: PROTOCOL,
            field: "response_format.type".into()
        }
    );
    assert!(
        matches!(
            decode_err(&with_extra(
                r#""response_format":{"type":"json_schema","json_schema":{"strict":true,"schema":{"type":"object"}}}"#
            )),
            CodecError::UnsupportedField { .. }
        ),
        "a strictness flag the canonical schema cannot carry is refused, not dropped"
    );
    assert!(
        matches!(
            decode_err(&with_extra(
                r#""response_format":{"type":"json_object","json_schema":{}}"#
            )),
            CodecError::InvalidRequest { .. }
        ),
        "a schema on a format that takes none is malformed"
    );
}
