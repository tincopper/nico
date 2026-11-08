//! Encoding a complete answer as one `GenerateContentResponse` document.

use nico_llm::{
    ChatResponse, CodecError, FinishReason, RawJson, ResponseId, ToolCall, ToolCallId, ToolName,
    protocol::ClientCodec,
};
use serde_json::Value;

use crate::support::{MODEL, codec, model, response, text_turn, usage};

/// A text answer encodes as one candidate with the model's content and a finish reason.
#[test]
fn a_text_answer_is_one_candidate_with_a_finish_reason() {
    let reply = codec()
        .encode_response(&response(text_turn("hello there"), FinishReason::Stop))
        .expect("a text answer encodes");
    assert_eq!(reply.status(), 200);

    let document: Value = serde_json::from_slice(reply.body()).expect("a JSON document");
    assert_eq!(document["responseId"], "resp-1");
    assert_eq!(document["modelVersion"], MODEL);
    let candidate = &document["candidates"][0];
    assert_eq!(candidate["content"]["role"], "model");
    assert_eq!(candidate["content"]["parts"][0]["text"], "hello there");
    assert_eq!(candidate["finishReason"], "STOP");
}

/// The finish-reason table is the adapter's read in reverse, with one spelling per canonical value.
#[test]
fn every_finish_reason_has_its_wire_spelling() {
    let wire = |reason| {
        let document = encode_value(&response(text_turn("x"), reason));
        document["candidates"][0]["finishReason"]
            .as_str()
            .expect("a reason")
            .to_owned()
    };
    assert_eq!(wire(FinishReason::Stop), "STOP");
    // `STOP` is also what this dialect reports after a function call: the calls are the content.
    assert_eq!(wire(FinishReason::ToolCalls), "STOP");
    assert_eq!(wire(FinishReason::Length), "MAX_TOKENS");
    assert_eq!(wire(FinishReason::ContentFilter), "SAFETY");
    assert_eq!(wire(FinishReason::Error), "OTHER");
}

/// A call, a thought and the usage counts all land on the document in the model's own order.
#[test]
fn calls_thoughts_and_usage_land_on_the_document() {
    let call = ToolCall::try_new(
        ToolCallId::try_new("call_0").expect("a valid id"),
        ToolName::try_new("lookup").expect("a valid name"),
        RawJson::parse(r#"{"q":"gemini"}"#).expect("valid arguments"),
    )
    .expect("a valid call");
    let turn = nico_llm::AssistantTurn::try_new(Vec::new(), vec![call])
        .expect("a valid turn")
        .with_reasoning(
            nico_llm::Reasoning::try_new("thinking", Some("sig-1".to_owned())).expect("valid"),
        );
    let mut answer = ChatResponse::try_new(
        ResponseId::try_new("resp-1").expect("a valid id"),
        model(),
        vec![nico_llm::Choice::try_new(0, turn, FinishReason::ToolCalls).expect("a valid choice")],
    )
    .expect("a valid response");
    answer = answer.with_usage(usage());

    let document = encode_value(&answer);
    let parts = &document["candidates"][0]["content"]["parts"];
    assert_eq!(parts[0]["text"], "thinking", "the thought comes first");
    assert_eq!(parts[0]["thought"], true);
    assert_eq!(parts[0]["thoughtSignature"], "sig-1");
    assert_eq!(parts[1]["functionCall"]["id"], "call_0");
    assert_eq!(parts[1]["functionCall"]["name"], "lookup");
    assert_eq!(parts[1]["functionCall"]["args"]["q"], "gemini");
    let usage = &document["usageMetadata"];
    assert_eq!(usage["promptTokenCount"], 11);
    assert_eq!(usage["candidatesTokenCount"], 7);
    assert_eq!(usage["cachedContentTokenCount"], 3);
    assert_eq!(usage["thoughtsTokenCount"], 2);
}

/// An answer whose turn is empty carries its finish reason with no content at all.
#[test]
fn a_filtered_answer_has_a_reason_and_no_content() {
    let turn = nico_llm::AssistantTurn::try_new(Vec::new(), Vec::new()).expect("an empty turn");
    let document = encode_value(&response(turn, FinishReason::ContentFilter));
    assert!(document["candidates"][0].get("content").is_none());
    assert_eq!(document["candidates"][0]["finishReason"], "SAFETY");
}

/// One candidate at index zero is all this protocol addresses.
#[test]
fn a_response_this_protocol_cannot_address_is_an_encode_failure() {
    let two = ChatResponse::try_new(
        ResponseId::try_new("resp-1").expect("a valid id"),
        model(),
        vec![
            nico_llm::Choice::try_new(0, text_turn("a"), FinishReason::Stop).expect("a choice"),
            nico_llm::Choice::try_new(1, text_turn("b"), FinishReason::Stop).expect("a choice"),
        ],
    )
    .expect("a valid response");
    let error = codec()
        .encode_response(&two)
        .expect_err("two candidates cannot be written");
    assert!(matches!(error, CodecError::Encode { .. }), "got {error:?}");

    let offset = ChatResponse::try_new(
        ResponseId::try_new("resp-1").expect("a valid id"),
        model(),
        vec![nico_llm::Choice::try_new(1, text_turn("a"), FinishReason::Stop).expect("a choice")],
    )
    .expect("a valid response");
    let error = codec()
        .encode_response(&offset)
        .expect_err("a candidate this protocol does not address");
    assert!(matches!(error, CodecError::Encode { .. }), "got {error:?}");
}

/// Preserved fields travel onto the document; a key beside a modelled member is refused.
#[test]
fn extensions_travel_and_shadowing_is_refused() {
    let mut labels = nico_llm::Extensions::new();
    labels
        .try_insert("labels", serde_json::json!({"team": "gateway"}))
        .expect("a valid extension");
    let document =
        encode_value(&response(text_turn("x"), FinishReason::Stop).with_extensions(labels));
    assert_eq!(
        document["labels"]["team"], "gateway",
        "the answer's own fields travel"
    );

    let mut shadow = nico_llm::Extensions::new();
    shadow
        .try_insert("responseId", serde_json::json!("sneaky"))
        .expect("the canonical model does not own this key");
    let error = codec()
        .encode_response(&response(text_turn("x"), FinishReason::Stop).with_extensions(shadow))
        .expect_err("a key beside a modelled member");
    assert!(matches!(error, CodecError::Encode { .. }), "got {error:?}");
}

/// Encodes one answer and parses the document.
fn encode_value(answer: &ChatResponse) -> Value {
    let reply = codec().encode_response(answer).expect("the answer encodes");
    serde_json::from_slice(reply.body()).expect("a JSON document")
}
