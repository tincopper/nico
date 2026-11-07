//! Decoding a `/v1/messages` request: the system field, content blocks, tools and refusals.

use nico_llm::{
    protocol::CodecError,
    schema::{ContentPart, ImageSource, Item, Role, Sampling, ToolChoice, ToolName},
};

use super::support::{canonical_request, decode, model, refusal, request, user};

#[test]
fn the_system_field_becomes_the_leading_system_message() {
    let body = r#"{
        "model": "claude-sonnet-4",
        "max_tokens": 64,
        "system": "be brief",
        "messages": [{"role": "user", "content": "hi"}]
    }"#;
    let request = decode(body).expect("the document is valid");
    assert_eq!(request.model(), &model());
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
fn a_system_array_is_a_system_message_too() {
    let body = r#"{
        "model": "claude-sonnet-4",
        "max_tokens": 64,
        "system": [{"type": "text", "text": "one"}, {"type": "text", "text": "two"}],
        "messages": [{"role": "user", "content": "hi"}]
    }"#;
    let request = decode(body).expect("the document is valid");
    match &request.conversation()[0] {
        Item::Message(message) => {
            assert_eq!(
                message.content(),
                [ContentPart::text("one"), ContentPart::text("two")]
            );
        }
        other => panic!("expected a system message, got {other:?}"),
    }
}

#[test]
fn an_assistant_turn_keeps_reasoning_text_and_calls_in_wire_order() {
    let body = r#"{
        "model": "claude-sonnet-4",
        "max_tokens": 64,
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "let me think", "signature": "sig-1"},
                {"type": "text", "text": "calling"},
                {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {"q": "x"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "found"}
            ]}
        ]
    }"#;
    let request = decode(body).expect("the document is valid");
    let conversation = request.conversation();
    assert_eq!(conversation.len(), 5, "one item per block run");
    assert!(matches!(conversation[0], Item::Message(_)));
    match &conversation[1] {
        Item::Reasoning(reasoning) => {
            assert_eq!(reasoning.text(), "let me think");
            assert_eq!(reasoning.signature(), Some("sig-1"));
        }
        other => panic!("expected reasoning, got {other:?}"),
    }
    match &conversation[2] {
        Item::Message(message) => {
            assert_eq!(message.role(), Role::Assistant);
            assert_eq!(message.content(), [ContentPart::text("calling")]);
        }
        other => panic!("expected the assistant text, got {other:?}"),
    }
    match &conversation[3] {
        Item::ToolCall(call) => {
            assert_eq!(call.id().as_str(), "toolu_1");
            assert_eq!(call.name().as_str(), "lookup");
        }
        other => panic!("expected a call, got {other:?}"),
    }
    match &conversation[4] {
        Item::ToolResult(result) => {
            assert_eq!(result.id().as_str(), "toolu_1");
            assert!(!result.is_error());
            assert_eq!(result.content(), [ContentPart::text("found")]);
        }
        other => panic!("expected a result, got {other:?}"),
    }
}

#[test]
fn an_inline_image_is_decoded_from_base64() {
    // "Hi!" — three bytes, so the decoder's padding path runs.
    let body = r#"{
        "model": "claude-sonnet-4",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "look"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "SGkh"}}
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
fn a_remote_image_keeps_its_url() {
    let body = r#"{
        "model": "claude-sonnet-4",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": [
            {"type": "image", "source": {"type": "url", "url": "https://example.test/a.png"}}
        ]}]
    }"#;
    let request = decode(body).expect("the document is valid");
    match &request.conversation()[0] {
        Item::Message(message) => assert_eq!(
            message.content(),
            [ContentPart::Image(ImageSource::Url(
                "https://example.test/a.png".to_owned()
            ))]
        ),
        other => panic!("expected a message, got {other:?}"),
    }
}

#[test]
fn tools_and_each_choice_are_mapped() {
    let body = r#"{
        "model": "claude-sonnet-4",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [{"name": "lookup", "description": "find", "input_schema": {"type": "object"}}],
        "tool_choice": {"type": "any"}
    }"#;
    let request = decode(body).expect("the document is valid");
    assert_eq!(request.tools().len(), 1);
    assert_eq!(request.tools()[0].name().as_str(), "lookup");
    assert_eq!(request.tools()[0].description(), Some("find"));
    assert_eq!(request.tool_choice(), &ToolChoice::Required);

    let body = body.replace(
        r#"{"type": "any"}"#,
        r#"{"type": "tool", "name": "lookup"}"#,
    );
    let request = decode(&body).expect("the document is valid");
    assert_eq!(
        request.tool_choice(),
        &ToolChoice::Tool(ToolName::try_new("lookup").expect("valid"))
    );

    let body = body.replace(
        r#"{"type": "tool", "name": "lookup"}"#,
        r#"{"type": "none"}"#,
    );
    let request = decode(&body).expect("the document is valid");
    assert_eq!(request.tool_choice(), &ToolChoice::None);
}

#[test]
fn a_parallel_tool_choice_is_refused_by_field() {
    let body = request("").replace(
        r#""messages""#,
        r#""tool_choice": {"type": "auto", "disable_parallel_tool_use": true}, "messages""#,
    );
    match refusal(&body) {
        CodecError::UnsupportedField { field, .. } => {
            assert_eq!(field, "tool_choice.disable_parallel_tool_use")
        }
        other => panic!("expected a field refusal, got {other:?}"),
    }
}

#[test]
fn sampling_carries_the_knobs_this_protocol_has() {
    let body = request(r#","temperature":0.5,"top_p":0.9,"stop_sequences":["\n\n"]"#);
    let request = decode(&body).expect("the document is valid");
    let sampling: &Sampling = request.sampling();
    assert_eq!(sampling.temperature(), Some(0.5));
    assert_eq!(sampling.top_p(), Some(0.9));
    assert_eq!(sampling.max_tokens(), Some(1024));
    assert_eq!(sampling.stop(), ["\n\n".to_owned()]);
}

#[test]
fn a_missing_max_tokens_is_a_malformed_document() {
    let body = r#"{"model":"claude-sonnet-4","messages":[{"role":"user","content":"hi"}]}"#;
    match refusal(body) {
        CodecError::InvalidRequest { reason, .. } => {
            assert!(
                reason.contains("max_tokens"),
                "reason names the field: {reason}"
            );
        }
        other => panic!("expected a malformed document, got {other:?}"),
    }
}

#[test]
fn a_temperature_above_one_is_refused_rather_than_clamped() {
    let body = request(r#","temperature":1.5"#);
    match refusal(&body) {
        CodecError::InvalidRequest { reason, .. } => {
            assert!(
                reason.contains("temperature"),
                "reason names the field: {reason}"
            );
        }
        other => panic!("expected a range refusal, got {other:?}"),
    }
}

#[test]
fn a_known_field_the_canonical_model_cannot_carry_is_refused_by_name() {
    for field in ["thinking", "top_k", "service_tier", "mcp_servers"] {
        let body = request(&format!(r#","{field}":true"#));
        match refusal(&body) {
            CodecError::UnsupportedField { field: named, .. } => assert_eq!(named, field),
            other => panic!("expected `{field}` to be refused, got {other:?}"),
        }
    }
}

#[test]
fn a_preserved_top_level_field_survives_the_decode() {
    let body = request(r#","metadata":{"user_id":"u-1"},"anthropic_beta":"x""#);
    let request = decode(&body).expect("the document is valid");
    assert!(request.extensions().get("metadata").is_some());
    assert!(request.extensions().get("anthropic_beta").is_some());
}

#[test]
fn an_unknown_block_type_is_a_malformed_document_and_a_known_one_is_refused() {
    let body = r#"{
        "model": "claude-sonnet-4",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": [{"type": "hologram", "data": "x"}]}]
    }"#;
    assert!(matches!(refusal(body), CodecError::InvalidRequest { .. }));

    let body = body.replace("hologram", "redacted_thinking");
    match refusal(&body) {
        CodecError::UnsupportedField { field, .. } => assert_eq!(field, "messages[0].content[0]"),
        other => panic!("expected a field refusal, got {other:?}"),
    }
}

#[test]
fn a_role_this_protocol_does_not_define_is_a_malformed_document() {
    let body =
        r#"{"model":"claude-sonnet-4","max_tokens":8,"messages":[{"role":"tool","content":"x"}]}"#;
    assert!(matches!(refusal(body), CodecError::InvalidRequest { .. }));
}

#[test]
fn the_canonical_request_matches_a_hand_built_one() {
    let body = r#"{"model":"claude-sonnet-4","max_tokens":1024,"messages":[{"role":"user","content":"hi"}]}"#;
    let request = decode(body).expect("the document is valid");
    let expected = canonical_request(vec![user("hi")]);
    assert_eq!(request.conversation(), expected.conversation());
    assert_eq!(request.model(), expected.model());
    assert!(!request.stream());
    assert_eq!(request.response_format(), expected.response_format());
    assert_eq!(
        request.sampling().max_tokens(),
        Some(1024),
        "the protocol's required ceiling reaches the canonical value"
    );
}
