//! What one canonical response becomes: the message, the tool calls, the usage and the extensions.

use nico_llm::{
    AssistantTurn, ClientCodec, ContentPart, Extensions, FinishReason, ImageSource, RawJson,
    Reasoning, TokenKind, ToolCall, ToolCallId, ToolName, Usage, UsageCounts,
};
use serde_json::json;

use super::support::{output, response, text_turn};

#[test]
fn a_text_answer_encodes_as_a_chat_completion() {
    let (status, document) = output(&response(text_turn("Two cats."), FinishReason::Stop));
    assert_eq!(status, 200, "a complete answer is a success");
    assert_eq!(document["object"], "chat.completion");
    assert_eq!(document["id"], "chatcmpl-1");
    assert_eq!(document["model"], "chat-small-2026-01-01");
    assert_eq!(document["created"], 0, "the encoder is deterministic");
    assert_eq!(document["choices"][0]["index"], 0);
    assert_eq!(document["choices"][0]["message"]["role"], "assistant");
    assert_eq!(document["choices"][0]["message"]["content"], "Two cats.");
    assert_eq!(document["choices"][0]["finish_reason"], "stop");
    assert!(
        document.get("usage").is_none(),
        "a response with no reported usage writes none"
    );
}

#[test]
fn every_finish_reason_has_a_wire_spelling() {
    for (reason, expected) in [
        (FinishReason::Stop, "stop"),
        (FinishReason::Length, "length"),
        (FinishReason::ToolCalls, "tool_calls"),
        (FinishReason::ContentFilter, "content_filter"),
    ] {
        let (_, document) = output(&response(text_turn("x"), reason));
        assert_eq!(document["choices"][0]["finish_reason"], expected);
    }
}

#[test]
fn tool_calls_and_reasoning_are_encoded_on_the_message() {
    let call = ToolCall::try_new(
        ToolCallId::try_new("call_1").expect("a valid call id"),
        ToolName::try_new("weather").expect("a valid tool name"),
        RawJson::parse(r#"{"city":"Paris"}"#).expect("arguments are JSON"),
    )
    .expect("a valid call");
    let turn = AssistantTurn::try_new(Vec::new(), vec![call])
        .expect("a valid turn")
        .with_reasoning(Reasoning::try_new("I should call the tool", None).expect("reasoning"));
    let (_, document) = output(&response(turn, FinishReason::ToolCalls));
    let message = &document["choices"][0]["message"];
    assert!(
        message["content"].is_null(),
        "a tool-only turn has no visible content"
    );
    assert_eq!(message["reasoning_content"], "I should call the tool");
    assert_eq!(message["tool_calls"][0]["id"], "call_1");
    assert_eq!(message["tool_calls"][0]["type"], "function");
    assert_eq!(message["tool_calls"][0]["function"]["name"], "weather");
    assert_eq!(
        message["tool_calls"][0]["function"]["arguments"], "{\"city\":\"Paris\"}",
        "the protocol carries the arguments as a JSON string"
    );
    assert_eq!(document["choices"][0]["finish_reason"], "tool_calls");
}

#[test]
fn usage_reports_the_provider_counts_and_nothing_else() {
    let counts = UsageCounts::new()
        .with(TokenKind::Input, 10)
        .with(TokenKind::Output, 4)
        .with(TokenKind::CacheRead, 8)
        .with(TokenKind::Reasoning, 2);
    let measured = UsageCounts::new().with(TokenKind::Input, 999);
    let response =
        response(text_turn("x"), FinishReason::Stop).with_usage(Usage::both(counts, measured));
    let (_, document) = output(&response);
    assert_eq!(document["usage"]["prompt_tokens"], 10);
    assert_eq!(document["usage"]["completion_tokens"], 4);
    assert_eq!(document["usage"]["total_tokens"], 14);
    assert_eq!(
        document["usage"]["prompt_tokens_details"]["cached_tokens"],
        8
    );
    assert_eq!(
        document["usage"]["completion_tokens_details"]["reasoning_tokens"],
        2
    );
    assert!(
        !document["usage"].to_string().contains("999"),
        "a measured count is not the provider's bill"
    );
}

#[test]
fn measured_only_usage_is_not_presented_as_a_bill() {
    let measured = Usage::measured(UsageCounts::new().with(TokenKind::Input, 10));
    let response = response(text_turn("x"), FinishReason::Stop).with_usage(measured);
    let (_, document) = output(&response);
    assert!(document.get("usage").is_none());
}

#[test]
fn a_preserved_response_field_is_written_next_to_the_modelled_ones() {
    let mut extensions = Extensions::new();
    extensions
        .try_insert("x_vendor", json!({"trace": "abc"}))
        .expect("a valid extension");
    let response = response(text_turn("x"), FinishReason::Stop).with_extensions(extensions);
    let (_, document) = output(&response);
    assert_eq!(document["x_vendor"], json!({"trace": "abc"}));
    assert_eq!(document["object"], "chat.completion");
}

#[test]
fn an_extension_key_that_would_shadow_a_modelled_field_is_refused() {
    let mut extensions = Extensions::new();
    extensions
        .try_insert("object", json!("sneaky"))
        .expect("the canonical model does not own this key");
    let response = response(text_turn("x"), FinishReason::Stop).with_extensions(extensions);
    response.validate().expect("the canonical shape accepts it");
    let error = super::support::codec()
        .encode_response(&response)
        .expect_err("the encoder must not write a duplicate field");
    assert!(
        matches!(&error, nico_llm::CodecError::Encode { reason, .. }
            if reason.contains("object")),
        "the refusal names the key: {error:?}"
    );
}

#[test]
fn a_part_the_document_cannot_express_fails_the_encoding() {
    let turn = AssistantTurn::try_new(
        vec![ContentPart::Image(ImageSource::Url(
            "https://example.test/cat.png".to_owned(),
        ))],
        Vec::new(),
    )
    .expect("the canonical model allows an image in a turn");
    let response = response(turn, FinishReason::Stop);
    let error = super::support::codec()
        .encode_response(&response)
        .expect_err("the protocol cannot write an image from the assistant");
    assert!(matches!(error, nico_llm::CodecError::Encode { .. }));
}

#[test]
fn an_empty_turn_is_an_empty_string() {
    let turn = AssistantTurn::try_new(Vec::new(), Vec::new()).expect("an empty turn");
    let (_, document) = output(&response(turn, FinishReason::Stop));
    assert_eq!(document["choices"][0]["message"]["content"], "");
}
