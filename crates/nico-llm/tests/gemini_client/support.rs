//! The documents and canonical values the Gemini codec's tests share.

use nico_llm::{
    ChatRequest, ClientCodec, CodecError, ModelId, RequestFacts,
    protocol::gemini::{GeminiGenerateContentCodec, PROTOCOL},
    schema::{
        AssistantTurn, ChatResponse, Choice, FinishReason, ResponseId, TokenKind, Usage,
        UsageCounts,
    },
};

/// The codec under test.
pub(crate) fn codec() -> GeminiGenerateContentCodec {
    GeminiGenerateContentCodec::new()
}

/// Facts of a request whose target names the fixture model and the buffered operation.
pub(crate) fn facts() -> RequestFacts {
    RequestFacts::new("POST", target(BUFFERED), Vec::new())
}

/// Facts of one explicit request target.
pub(crate) fn facts_at(target: &str) -> RequestFacts {
    RequestFacts::new("POST", target, Vec::new())
}

/// The operation spellings the target parser knows.
pub(crate) const BUFFERED: &str = "generateContent";
pub(crate) const STREAMING: &str = "streamGenerateContent?alt=sse";

/// Builds the request target for one operation spelling.
pub(crate) fn target(operation: &str) -> String {
    format!("/v1beta/models/{MODEL}:{operation}")
}

/// The wire name of the protocol under test.
pub(crate) const PROTOCOL_NAME: &str = "gemini.generate_content";

/// The model name the fixtures write into the path.
pub(crate) const MODEL: &str = "gemini-2.0-flash";

/// The canonical model the fixtures decode to: the path's own model.
pub(crate) fn model() -> ModelId {
    ModelId::try_new(MODEL).expect("a valid model name")
}

/// Decodes one request body against the buffered target.
pub(crate) fn decode(body: &str) -> Result<ChatRequest, CodecError> {
    decode_at(&facts(), body)
}

/// Decodes one request body against an explicit target.
pub(crate) fn decode_at(facts: &RequestFacts, body: &str) -> Result<ChatRequest, CodecError> {
    codec().decode_request(facts, body.as_bytes())
}

/// Decodes one body and reports only the refusal.
pub(crate) fn refusal(body: &str) -> CodecError {
    decode(body).expect_err("the document must be refused")
}

/// One response with a single assistant turn.
pub(crate) fn response(turn: AssistantTurn, reason: FinishReason) -> ChatResponse {
    ChatResponse::try_new(
        ResponseId::try_new("resp-1").expect("a valid id"),
        model(),
        vec![Choice::try_new(0, turn, reason).expect("a valid choice")],
    )
    .expect("a valid response")
}

/// Reported usage with all four buckets the dialect reports.
pub(crate) fn usage() -> Usage {
    Usage::reported(
        UsageCounts::new()
            .with(TokenKind::Input, 11)
            .with(TokenKind::Output, 7)
            .with(TokenKind::CacheRead, 3)
            .with(TokenKind::Reasoning, 2),
    )
}

/// An assistant turn with visible text only.
pub(crate) fn text_turn(text: &str) -> AssistantTurn {
    AssistantTurn::try_new(vec![nico_llm::ContentPart::text(text)], Vec::new())
        .expect("a valid turn")
}

/// Reports the protocol identifier this codec answers to.
pub(crate) fn protocol_name() -> &'static str {
    PROTOCOL.as_str()
}

/// Keeps the protocol-name assertion honest about the constant it compares.
#[test]
fn the_codec_registers_the_protocol_its_name_promises() {
    assert_eq!(protocol_name(), PROTOCOL_NAME);
}
