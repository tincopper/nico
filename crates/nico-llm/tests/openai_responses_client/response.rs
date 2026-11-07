//! Encoding canonical responses as `response` documents.

use nico_llm::{
    AssistantTurn, ClientCodec,
    protocol::CodecError,
    schema::{ChatResponse, Choice, FinishReason, ResponseId},
};

use super::support::{call, model, response, text_turn, usage};

/// Encodes one response and returns the parsed document.
fn document(response: &ChatResponse) -> serde_json::Value {
    let bytes = super::support::codec()
        .encode_response(response)
        .expect("the response is encodable");
    assert_eq!(bytes.status(), 200);
    serde_json::from_slice(bytes.body()).expect("the body is a JSON document")
}

#[test]
fn a_text_answer_is_one_output_message() {
    let response = response(text_turn("hello"), FinishReason::Stop).with_usage(usage());
    let document = document(&response);

    assert_eq!(document["object"], "response");
    assert_eq!(document["status"], "completed");
    assert!(document.get("incomplete_details").is_none());
    assert_eq!(document["output"][0]["type"], "message");
    assert_eq!(document["output"][0]["role"], "assistant");
    assert_eq!(document["output"][0]["status"], "completed");
    assert_eq!(document["output"][0]["content"][0]["type"], "output_text");
    assert_eq!(document["output"][0]["content"][0]["text"], "hello");
    assert_eq!(document["usage"]["input_tokens"], 11);
    assert_eq!(document["usage"]["output_tokens"], 7);
    assert_eq!(document["usage"]["total_tokens"], 18);
    assert_eq!(
        document["usage"]["input_tokens_details"]["cached_tokens"],
        3
    );
}

#[test]
fn a_call_is_an_item_of_its_own() {
    let turn = AssistantTurn::try_new(Vec::new(), vec![call("call_1", "lookup", r#"{"q":"x"}"#)])
        .expect("a valid turn");
    let document = document(&response(turn, FinishReason::ToolCalls));
    assert_eq!(document["status"], "completed");
    assert_eq!(document["output"][0]["type"], "function_call");
    assert_eq!(document["output"][0]["call_id"], "call_1");
    assert_eq!(document["output"][0]["name"], "lookup");
    assert_eq!(document["output"][0]["arguments"], r#"{"q":"x"}"#);
}

#[test]
fn a_truncated_answer_reports_why() {
    for (reason, expected) in [
        (FinishReason::Length, "max_output_tokens"),
        (FinishReason::ContentFilter, "content_filter"),
    ] {
        let document = document(&response(text_turn("x"), reason));
        assert_eq!(document["status"], "incomplete", "{reason:?}");
        assert_eq!(document["incomplete_details"]["reason"], expected);
    }
}

#[test]
fn two_candidates_are_a_value_this_protocol_cannot_write() {
    let response = ChatResponse::try_new(
        ResponseId::try_new("resp_01").expect("a valid id"),
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
fn a_preserved_extension_is_written_next_to_the_modelled_members() {
    let mut extensions = nico_llm::Extensions::new();
    extensions
        .try_insert(
            "note",
            nico_llm::RawJson::parse(r#""kept""#).expect("valid JSON"),
        )
        .expect("a valid key");
    let document =
        document(&response(text_turn("x"), FinishReason::Stop).with_extensions(extensions));
    assert_eq!(document["note"], "kept");
}
