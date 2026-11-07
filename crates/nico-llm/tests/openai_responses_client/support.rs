//! The documents and canonical values the Responses codec's tests share.

use nico_llm::{
    protocol::{
        ClientCodec, CodecError, RequestFacts,
        openai_responses::{OpenAiResponsesCodec, PROTOCOL},
    },
    schema::{
        AssistantTurn, ChatResponse, Choice, ContentPart, FinishReason, Message, ModelId, RawJson,
        ResponseId, TokenKind, ToolCall, ToolCallId, ToolName, Usage, UsageCounts,
    },
};

/// The codec under test.
pub(crate) fn codec() -> OpenAiResponsesCodec {
    OpenAiResponsesCodec::new()
}

/// Facts of a request that carries everything in its body.
pub(crate) fn facts() -> RequestFacts {
    RequestFacts::new("POST", "/v1/responses", Vec::new())
}

/// Decodes one request body.
pub(crate) fn decode(body: &str) -> Result<nico_llm::ChatRequest, CodecError> {
    codec().decode_request(&facts(), body.as_bytes())
}

/// Decodes one body and reports only the refusal.
pub(crate) fn refusal(body: &str) -> CodecError {
    decode(body).expect_err("the document must be refused")
}

/// The wire name of the protocol under test.
pub(crate) const PROTOCOL_NAME: &str = "openai.responses";

/// The canonical model name the fixtures use.
pub(crate) fn model() -> ModelId {
    ModelId::try_new("gpt-5").expect("a valid model name")
}

/// One response with a single assistant turn.
pub(crate) fn response(turn: AssistantTurn, reason: FinishReason) -> ChatResponse {
    ChatResponse::try_new(
        ResponseId::try_new("resp_01").expect("a valid id"),
        model(),
        vec![Choice::try_new(0, turn, reason).expect("a valid choice")],
    )
    .expect("a valid response")
}

/// Reported usage with input, output and a cached count.
pub(crate) fn usage() -> Usage {
    Usage::reported(
        UsageCounts::new()
            .with(TokenKind::Input, 11)
            .with(TokenKind::Output, 7)
            .with(TokenKind::CacheRead, 3),
    )
}

/// An assistant turn with visible text only.
pub(crate) fn text_turn(text: &str) -> AssistantTurn {
    AssistantTurn::try_new(vec![ContentPart::text(text)], Vec::new()).expect("a valid turn")
}

/// One tool call.
pub(crate) fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
    ToolCall::try_new(
        ToolCallId::try_new(id).expect("a valid id"),
        ToolName::try_new(name).expect("a valid name"),
        RawJson::parse(arguments).expect("valid arguments"),
    )
    .expect("a valid call")
}

/// A user message item.
pub(crate) fn user(text: &str) -> nico_llm::Item {
    nico_llm::Item::Message(Message::user(text))
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
