//! Encoding canonical responses as `message` documents.

use nico_llm::{
    AssistantTurn, ClientCodec,
    protocol::CodecError,
    schema::{ChatResponse, Choice, ContentPart, Extensions, FinishReason, RawJson, ResponseId},
};

use super::support::{call, model, reasoning, response, text_turn, usage};

#[test]
fn a_text_answer_carries_one_text_block_and_its_usage() {
    let response = response(text_turn("hello"), FinishReason::Stop).with_usage(usage());
    let bytes = super::support::codec()
        .encode_response(&response)
        .expect("the response is encodable");
    assert_eq!(bytes.status(), 200);
    let document: serde_json::Value =
        serde_json::from_slice(bytes.body()).expect("the body is a JSON document");

    assert_eq!(document["type"], "message");
    assert_eq!(document["role"], "assistant");
    assert_eq!(document["model"], "claude-sonnet-4");
    assert_eq!(document["stop_reason"], "end_turn");
    assert!(
        document["stop_sequence"].is_null(),
        "the member is always present"
    );
    assert_eq!(document["content"][0]["type"], "text");
    assert_eq!(document["content"][0]["text"], "hello");
    assert_eq!(document["usage"]["input_tokens"], 11);
    assert_eq!(document["usage"]["output_tokens"], 7);
    assert_eq!(document["usage"]["cache_read_input_tokens"], 3);
    assert!(
        document["usage"]
            .get("cache_creation_input_tokens")
            .is_none(),
        "an unreported cache write has no field, rather than zero"
    );
}

#[test]
fn reasoning_and_calls_become_blocks_in_canonical_order() {
    let turn = AssistantTurn::try_new(
        vec![ContentPart::text("calling")],
        vec![call("toolu_1", "lookup", r#"{"q":"x"}"#)],
    )
    .expect("a valid turn")
    .with_reasoning(reasoning("thinking", Some("sig-1")));
    let response = response(turn, FinishReason::ToolCalls);
    let bytes = super::support::codec()
        .encode_response(&response)
        .expect("the response is encodable");
    let document: serde_json::Value =
        serde_json::from_slice(bytes.body()).expect("the body is a JSON document");

    assert_eq!(document["stop_reason"], "tool_use");
    let content = document["content"].as_array().expect("content is an array");
    assert_eq!(content.len(), 3);
    assert_eq!(content[0]["type"], "thinking");
    assert_eq!(content[0]["thinking"], "thinking");
    assert_eq!(content[0]["signature"], "sig-1");
    assert_eq!(content[1]["type"], "text");
    assert_eq!(content[2]["type"], "tool_use");
    assert_eq!(content[2]["id"], "toolu_1");
    assert_eq!(content[2]["name"], "lookup");
    assert_eq!(content[2]["input"]["q"], "x");
}

#[test]
fn a_turn_without_visible_text_writes_no_text_block() {
    let turn = AssistantTurn::try_new(Vec::new(), vec![call("toolu_1", "lookup", "{}")])
        .expect("a valid turn");
    let bytes = super::support::codec()
        .encode_response(&response(turn, FinishReason::ToolCalls))
        .expect("the response is encodable");
    let document: serde_json::Value =
        serde_json::from_slice(bytes.body()).expect("the body is a JSON document");
    assert_eq!(document["content"][0]["type"], "tool_use");
}

#[test]
fn every_finish_reason_has_a_spelling() {
    for (reason, expected) in [
        (FinishReason::Stop, "end_turn"),
        (FinishReason::Length, "max_tokens"),
        (FinishReason::ToolCalls, "tool_use"),
        (FinishReason::ContentFilter, "refusal"),
        (FinishReason::Error, "end_turn"),
    ] {
        let bytes = super::support::codec()
            .encode_response(&response(text_turn("x"), reason))
            .expect("the response is encodable");
        let document: serde_json::Value =
            serde_json::from_slice(bytes.body()).expect("the body is a JSON document");
        assert_eq!(document["stop_reason"], expected, "{reason:?}");
    }
}

#[test]
fn two_candidates_are_a_value_this_protocol_cannot_write() {
    let response = ChatResponse::try_new(
        ResponseId::try_new("msg_01").expect("a valid id"),
        model(),
        vec![
            Choice::try_new(0, text_turn("one"), FinishReason::Stop).expect("a valid choice"),
            Choice::try_new(1, text_turn("two"), FinishReason::Stop).expect("a valid choice"),
        ],
    )
    .expect("a valid response");
    assert!(matches!(
        super::support::codec().encode_response(&response),
        Err(CodecError::Encode { .. })
    ));
}

#[test]
fn an_extension_key_that_shadows_a_member_is_refused() {
    let mut extensions = Extensions::new();
    extensions
        .try_insert("usage", RawJson::parse("{}").expect("valid JSON"))
        .expect("a valid key");
    let response = response(text_turn("x"), FinishReason::Stop).with_extensions(extensions);
    assert!(matches!(
        super::support::codec().encode_response(&response),
        Err(CodecError::Encode { .. })
    ));
}

#[test]
fn a_preserved_extension_is_written_next_to_the_modelled_members() {
    let mut extensions = Extensions::new();
    extensions
        .try_insert("note", RawJson::parse(r#""kept""#).expect("valid JSON"))
        .expect("a valid key");
    let response = response(text_turn("x"), FinishReason::Stop).with_extensions(extensions);
    let bytes = super::support::codec()
        .encode_response(&response)
        .expect("the response is encodable");
    let document: serde_json::Value =
        serde_json::from_slice(bytes.body()).expect("the body is a JSON document");
    assert_eq!(document["note"], "kept");
}
