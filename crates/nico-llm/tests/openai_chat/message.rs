//! What one message becomes: content parts, tool results, and the nested fields that are refused.

use nico_llm::{CodecError, ContentPart, ImageSource, Item, Role};

use super::support::{decode, decode_err};

#[test]
fn a_tool_result_is_an_item_with_the_call_id() {
    let request = decode(
        r#"{"model":"m","messages":[
            {"role":"user","content":"weather?"},
            {"role":"tool","tool_call_id":"call_1","content":"22 degrees"}
        ]}"#,
    );
    assert_eq!(request.conversation().len(), 2);
    match &request.conversation()[1] {
        Item::ToolResult(result) => {
            assert_eq!(result.id().as_str(), "call_1");
            assert_eq!(result.content(), [ContentPart::text("22 degrees")]);
            assert!(
                !result.is_error(),
                "a tool result the wire did not mark as failed must not claim it did"
            );
        }
        other => panic!("a tool message is a result, not a message: {other:?}"),
    }
}

#[test]
fn an_array_content_decodes_text_and_a_remote_image() {
    let request = decode(
        r#"{"model":"m","messages":[{"role":"user","content":[
            {"type":"text","text":"what is this?"},
            {"type":"image_url","image_url":{"url":"https://example.test/cat.png"}}
        ]}]}"#,
    );
    match &request.conversation()[0] {
        Item::Message(message) => {
            assert_eq!(message.content()[0], ContentPart::text("what is this?"));
            match &message.content()[1] {
                ContentPart::Image(ImageSource::Url(url)) => {
                    assert_eq!(url, "https://example.test/cat.png");
                }
                other => panic!("expected a remote image, got {other:?}"),
            }
        }
        other => panic!("expected a message, got {other:?}"),
    }
}

#[test]
fn an_inline_image_is_refused_by_field() {
    let error = decode_err(
        r#"{"model":"m","messages":[{"role":"user","content":[
            {"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}
        ]}]}"#,
    );
    assert_eq!(
        error,
        CodecError::UnsupportedField {
            protocol: nico_llm::protocol::openai_chat::PROTOCOL,
            field: "messages[0].content[0].image_url.url".into()
        }
    );
}

#[test]
fn an_image_rendering_hint_is_refused_by_field() {
    let error = decode_err(
        r#"{"model":"m","messages":[{"role":"user","content":[
            {"type":"image_url","image_url":{"url":"https://example.test/cat.png","detail":"high"}}
        ]}]}"#,
    );
    assert!(
        matches!(&error, CodecError::UnsupportedField { field, .. } if field == "messages[0].content[0].image_url.detail"),
        "the hint changes what the provider sees: {error:?}"
    );
}

#[test]
fn a_file_part_is_refused_by_capability_and_audio_by_field() {
    assert_eq!(
        decode_err(
            r#"{"model":"m","messages":[{"role":"user","content":[
                {"type":"file","file":{"file_id":"file_1"}}
            ]}]}"#
        ),
        CodecError::Unsupported {
            protocol: nico_llm::protocol::openai_chat::PROTOCOL,
            feature: nico_llm::Capability::Files
        }
    );
    assert!(
        matches!(
            decode_err(
                r#"{"model":"m","messages":[{"role":"user","content":[
                    {"type":"input_audio","input_audio":{"data":"AAAA","format":"wav"}}
                ]}]}"#
            ),
            CodecError::UnsupportedField { field, .. } if field == "messages[0].content[0]"
        ),
        "audio has no capability to name, so the part is refused by path"
    );
}

#[test]
fn a_part_type_this_gateway_does_not_know_is_refused_with_its_path() {
    let error = decode_err(
        r#"{"model":"m","messages":[{"role":"user","content":[
            {"type":"video_url","video_url":{"url":"https://example.test/clip.mp4"}}
        ]}]}"#,
    );
    assert!(
        matches!(&error, CodecError::InvalidRequest { reason, .. }
            if reason.contains("messages[0].content[0].type")),
        "the refusal must name the part: {error:?}"
    );
}

#[test]
fn an_unknown_member_of_a_modelled_object_is_refused_with_its_path() {
    let error =
        decode_err(r#"{"model":"m","messages":[{"role":"user","content":"hi","nickname":"bob"}]}"#);
    assert!(
        matches!(&error, CodecError::InvalidRequest { reason, .. }
            if reason.contains("messages[0].nickname")),
        "the refusal must name the member: {error:?}"
    );

    let error = decode_err(
        r#"{"model":"m","messages":[{"role":"user","content":[
            {"type":"text","text":"hi","x_extra":true}
        ]}]}"#,
    );
    assert!(
        matches!(&error, CodecError::InvalidRequest { reason, .. }
            if reason.contains("messages[0].content[0].x_extra")),
        "the refusal must name the member: {error:?}"
    );
}

#[test]
fn an_unprintable_unknown_member_is_refused_without_being_quoted() {
    // A member name is the caller's text too, and the refusal reaches a log record: a name carrying
    // a newline (or kilobytes of data) must not be able to forge or bloat that record.
    let error = decode_err(
        r#"{"model":"m","messages":[{"role":"user","content":"hi","evil\nkey":"MARKER"}]}"#,
    );
    assert!(
        matches!(&error, CodecError::InvalidRequest { reason, .. }
            if !reason.contains("evil") && reason.contains("does not model")),
        "the member must be refused without being quoted: {error:?}"
    );
}

#[test]
fn a_message_field_the_canonical_model_lacks_is_refused_by_name() {
    for (member, field) in [
        (r#""name":"bob""#, "messages[0].name"),
        (r#""refusal":"I cannot""#, "messages[0].refusal"),
    ] {
        let body = format!(
            r#"{{"model":"m","messages":[{{"role":"assistant","content":"hi",{member}}}]}}"#
        );
        assert_eq!(
            decode_err(&body),
            CodecError::UnsupportedField {
                protocol: nico_llm::protocol::openai_chat::PROTOCOL,
                field: field.into()
            },
            "the message member {member} must be refused"
        );
    }
}

#[test]
fn an_assistant_with_neither_content_nor_calls_is_refused() {
    assert!(matches!(
        decode_err(r#"{"model":"m","messages":[{"role":"assistant","content":null}]}"#),
        CodecError::InvalidRequest { .. }
    ));
}

#[test]
fn a_tool_message_needs_the_call_it_answers() {
    assert!(matches!(
        decode_err(r#"{"model":"m","messages":[{"role":"tool","content":"answer"}]}"#),
        CodecError::InvalidRequest { .. }
    ));
    assert!(matches!(
        decode_err(
            r#"{"model":"m","messages":[{"role":"tool","tool_call_id":"c1","content":"x","tool_calls":[]}]}"#
        ),
        CodecError::InvalidRequest { .. }
    ));
}

#[test]
fn tool_arguments_must_be_a_json_object() {
    let error = decode_err(
        r#"{"model":"m","messages":[
            {"role":"assistant","content":null,"tool_calls":[
                {"id":"c1","type":"function","function":{"name":"f","arguments":"not json"}}
            ]}
        ]}"#,
    );
    assert!(
        matches!(&error, CodecError::InvalidRequest { reason, .. }
            if reason.contains("arguments")),
        "the refusal must name the arguments: {error:?}"
    );
    assert!(matches!(
        decode_err(
            r#"{"model":"m","messages":[
                {"role":"assistant","content":null,"tool_calls":[
                    {"id":"c1","type":"function","function":{"name":"f","arguments":"[1]"}}
                ]}
            ]}"#
        ),
        CodecError::InvalidRequest { .. }
    ));
}

#[test]
fn a_role_this_gateway_does_not_know_is_refused() {
    assert!(matches!(
        decode_err(r#"{"model":"m","messages":[{"role":"critic","content":"hm"}]}"#),
        CodecError::InvalidRequest { .. }
    ));
    assert!(
        matches!(
            decode_err(r#"{"model":"m","messages":[{"role":"function","content":"hm"}]}"#),
            CodecError::UnsupportedField { .. }
        ),
        "the legacy function role is known and refused by name"
    );
    let request = decode(r#"{"model":"m","messages":[{"role":"assistant","content":"hi"}]}"#);
    assert!(
        matches!(&request.conversation()[0], Item::Message(message) if message.role() == Role::Assistant)
    );
}

#[test]
fn a_member_that_belongs_to_another_role_is_refused() {
    for (body, field) in [
        (
            r#"{"model":"m","messages":[{"role":"user","content":"hi","tool_call_id":"c1"}]}"#,
            "messages[0].tool_call_id",
        ),
        (
            r#"{"model":"m","messages":[{"role":"user","content":"hi","tool_calls":[]}]}"#,
            "messages[0].tool_calls",
        ),
        (
            r#"{"model":"m","messages":[{"role":"user","content":"hi","reasoning_content":"hmm"}]}"#,
            "messages[0].reasoning_content",
        ),
    ] {
        assert!(
            matches!(&decode_err(body), CodecError::InvalidRequest { reason, .. } if reason.contains(field)),
            "a member of another role must be refused by name: {field}"
        );
    }
}

#[test]
fn a_part_carrying_another_parts_member_is_refused() {
    assert!(matches!(
        decode_err(
            r#"{"model":"m","messages":[{"role":"user","content":[
                {"type":"text","text":"hi","image_url":{"url":"https://example.test/cat.png"}}
            ]}]}"#
        ),
        CodecError::InvalidRequest { .. }
    ));
    assert!(matches!(
        decode_err(
            r#"{"model":"m","messages":[{"role":"user","content":[
                {"type":"image_url","text":"hi","image_url":{"url":"https://example.test/cat.png"}}
            ]}]}"#
        ),
        CodecError::InvalidRequest { .. }
    ));
}
