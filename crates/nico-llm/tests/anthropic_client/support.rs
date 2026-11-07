//! The documents and canonical values the Anthropic codec's tests share.

use nico_llm::{
    protocol::{ClientCodec, CodecError, RequestFacts, anthropic::AnthropicMessagesCodec},
    schema::{
        AssistantTurn, ChatRequest, ChatResponse, Choice, ContentPart, FinishReason, Item, Message,
        ModelId, RawJson, Reasoning, ResponseId, TokenKind, ToolCall, ToolCallId, ToolName, Usage,
        UsageCounts,
    },
};

/// The codec under test.
pub(crate) fn codec() -> AnthropicMessagesCodec {
    AnthropicMessagesCodec::new()
}

/// Facts of a request that carries everything in its body.
pub(crate) fn facts() -> RequestFacts {
    RequestFacts::new("POST", "/v1/messages", Vec::new())
}

/// Decodes one request body.
///
/// # Errors
/// Returns the codec's refusal, so a test can assert which one it is.
pub(crate) fn decode(body: &str) -> Result<ChatRequest, CodecError> {
    codec().decode_request(&facts(), body.as_bytes())
}

/// Decodes one body and reports only the refusal.
pub(crate) fn refusal(body: &str) -> CodecError {
    decode(body).expect_err("the document must be refused")
}

/// A minimal valid request body, with `extra` merged into the top level by the caller's raw text.
pub(crate) fn request(extra: &str) -> String {
    format!(
        r#"{{"model":"claude-sonnet-4","max_tokens":1024,"messages":[{{"role":"user","content":"hi"}}]{extra}}}"#
    )
}

/// The canonical model name the fixtures use.
pub(crate) fn model() -> ModelId {
    ModelId::try_new("claude-sonnet-4").expect("a valid model name")
}

/// A canonical request with one user message, for the decode-equality assertions.
pub(crate) fn canonical_request(items: Vec<Item>) -> ChatRequest {
    ChatRequest::try_new(model(), items).expect("a valid conversation")
}

/// One user message item.
pub(crate) fn user(text: &str) -> Item {
    Item::Message(Message::user(text))
}

/// One response with a single assistant turn.
pub(crate) fn response(turn: AssistantTurn, reason: FinishReason) -> ChatResponse {
    ChatResponse::try_new(
        ResponseId::try_new("msg_01").expect("a valid id"),
        model(),
        vec![Choice::try_new(0, turn, reason).expect("a valid choice")],
    )
    .expect("a valid response")
}

/// Reported usage with input and output tokens.
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

/// One reasoning item.
pub(crate) fn reasoning(text: &str, signature: Option<&str>) -> Reasoning {
    Reasoning::try_new(text, signature.map(str::to_owned)).expect("valid reasoning")
}
