//! The canonical conversation every wire fixture translates, and what "equal" means here.
//!
//! One conversation and one answer are the whole point of the matrix: if four client codecs and
//! four provider adapters translate the *same* canonical values, any pairwise combination is a
//! composition of translations this file has already checked. The canonical side is built through
//! the schema constructors, so a fixture that cannot decode into it is a broken fixture, not a
//! silently accepted one.
//!
//! Equality is semantic, not structural: a codec may carry a fact in its own spelling, and the
//! fields each protocol keeps for itself (`extensions`) are exactly the ones two protocols cannot
//! share, so they are excluded from every comparison here.

use nico_llm::{
    AssistantTurn, ChatRequest, ChatResponse, Choice, ContentPart, FinishReason, ImageSource, Item,
    Message, ModelId, RawJson, Reasoning, ResponseId, Role, Sampling, TokenKind, Tool, ToolCall,
    ToolCallId, ToolChoice, ToolName, ToolResult, Usage, UsageCounts,
};

/// The logical model every client request names.
pub(crate) const LOGICAL: &str = "fast";

/// The physical model every provider document names.
pub(crate) const PHYSICAL: &str = "phys-model";

/// The response id every answer carries.
pub(crate) const RESPONSE_ID: &str = "resp-1";

/// The prompt of the conversation.
pub(crate) const PROMPT: &str = "look at this";

/// The inline image's bytes: `hello`, which every wire document carries as `aGVsbG8=`.
const IMAGE_BYTES: &[u8] = b"hello";

/// The assistant text before the call.
pub(crate) const CALL_TEXT: &str = "calling";

/// The tool the conversation declares, calls and answers.
pub(crate) const TOOL_NAME: &str = "lookup";

/// The call id the conversation carries.
pub(crate) const CALL_ID: &str = "call_0";

/// The answer text of the response.
pub(crate) const ANSWER: &str = "the answer";

/// The reasoning text of the response.
///
/// The canonical reasoning carries no signature here, on purpose: the four providers do not agree on
/// one (the OpenAI dialects have none, and the Gemini fold drops the signature), so a signature
/// would make one answer impossible to serve on every row.
pub(crate) const REASONING: &str = "thinking";

/// Builds the one canonical request the matrix translates.
pub(crate) fn canonical_request() -> ChatRequest {
    let model = ModelId::try_new(LOGICAL).expect("a valid model name");
    let conversation = vec![
        Item::Message(
            Message::try_new(Role::System, vec![ContentPart::text("be brief")])
                .expect("a valid message"),
        ),
        Item::Message(
            Message::try_new(
                Role::User,
                vec![
                    ContentPart::text(PROMPT),
                    ContentPart::Image(ImageSource::Base64 {
                        media_type: "image/png".to_owned(),
                        data: IMAGE_BYTES.to_vec(),
                    }),
                ],
            )
            .expect("a valid message"),
        ),
        Item::Message(
            Message::try_new(Role::Assistant, vec![ContentPart::text(CALL_TEXT)])
                .expect("a valid message"),
        ),
        Item::ToolCall(
            ToolCall::try_new(
                ToolCallId::try_new(CALL_ID).expect("a valid id"),
                ToolName::try_new(TOOL_NAME).expect("a valid name"),
                RawJson::parse(r#"{"q":"gemini"}"#).expect("valid arguments"),
            )
            .expect("a valid call"),
        ),
        Item::ToolResult(
            ToolResult::try_new(
                ToolCallId::try_new(CALL_ID).expect("a valid id"),
                vec![ContentPart::text("found")],
            )
            .expect("a valid result"),
        ),
    ];
    let tool = Tool::try_new(
        ToolName::try_new(TOOL_NAME).expect("a valid name"),
        RawJson::parse(r#"{"type":"object"}"#).expect("valid parameters"),
    )
    .expect("a valid tool")
    .with_description("finds")
    .expect("a valid description");
    ChatRequest::try_new(model, conversation)
        .expect("a valid request")
        .with_tools(vec![tool])
        .expect("a valid tool set")
        .with_tool_choice(ToolChoice::Auto)
        .expect("a valid choice")
        .with_sampling(Sampling::try_new(None, None, Some(64)).expect("a valid sampling"))
}

/// Builds the one canonical response the matrix translates.
pub(crate) fn canonical_response() -> ChatResponse {
    let turn = AssistantTurn::try_new(vec![ContentPart::text(ANSWER)], Vec::new())
        .expect("a valid turn")
        .with_reasoning(Reasoning::try_new(REASONING, None).expect("a valid reasoning"));
    ChatResponse::try_new(
        ResponseId::try_new(RESPONSE_ID).expect("a valid id"),
        ModelId::try_new(PHYSICAL).expect("a valid model name"),
        vec![Choice::try_new(0, turn, FinishReason::Stop).expect("a valid choice")],
    )
    .expect("a valid response")
    .with_usage(Usage::reported(
        UsageCounts::new()
            .with(TokenKind::Input, 11)
            .with(TokenKind::Output, 7)
            .with(TokenKind::CacheRead, 3),
    ))
}

/// Asserts a decoded request *is* the canonical conversation, field by field.
///
/// `extensions` is deliberately not compared: it carries each protocol's own preserved fields, which
/// are the one thing two protocols cannot share.
pub(crate) fn assert_request_translates(decoded: &ChatRequest) {
    let canonical = canonical_request();
    assert_eq!(
        decoded.model(),
        canonical.model(),
        "the model the client named"
    );
    assert_eq!(decoded.stream(), canonical.stream(), "the stream flag");
    assert_eq!(
        decoded.sampling().max_tokens(),
        canonical.sampling().max_tokens(),
        "the output ceiling"
    );
    assert_eq!(
        decoded.tool_choice(),
        canonical.tool_choice(),
        "the tool choice"
    );
    assert_eq!(
        decoded.tools().len(),
        canonical.tools().len(),
        "the number of declared tools"
    );
    for (decoded, canonical) in decoded.tools().iter().zip(canonical.tools()) {
        assert_eq!(decoded.name(), canonical.name(), "the tool name");
        assert_eq!(
            decoded.description(),
            canonical.description(),
            "the tool description"
        );
        assert_eq!(
            decoded.parameters(),
            canonical.parameters(),
            "the parameters"
        );
    }
    assert!(
        matches!(decoded.response_format(), nico_llm::ResponseFormat::Text),
        "the response format is plain text"
    );
    assert_conversation(decoded.conversation(), canonical.conversation());
}

/// Asserts two conversations carry the same items in the same order.
fn assert_conversation(decoded: &[Item], canonical: &[Item]) {
    assert_eq!(
        decoded.len(),
        canonical.len(),
        "the conversation carries the canonical items in order"
    );
    for (index, (decoded, canonical)) in decoded.iter().zip(canonical).enumerate() {
        match (decoded, canonical) {
            (Item::Message(decoded), Item::Message(canonical)) => {
                assert_eq!(decoded.role(), canonical.role(), "item {index}: the role");
                assert_eq!(
                    decoded.content(),
                    canonical.content(),
                    "item {index}: the parts"
                );
            }
            (Item::ToolCall(decoded), Item::ToolCall(canonical)) => {
                assert_eq!(decoded.id(), canonical.id(), "item {index}: the call id");
                assert_eq!(decoded.name(), canonical.name(), "item {index}: the tool");
                assert_eq!(
                    decoded.arguments(),
                    canonical.arguments(),
                    "item {index}: the arguments"
                );
            }
            (Item::ToolResult(decoded), Item::ToolResult(canonical)) => {
                assert_eq!(decoded.id(), canonical.id(), "item {index}: the result id");
                assert_eq!(
                    decoded.is_error(),
                    canonical.is_error(),
                    "item {index}: the failure flag"
                );
                assert_eq!(
                    decoded.content(),
                    canonical.content(),
                    "item {index}: the result content"
                );
            }
            (Item::Reasoning(decoded), Item::Reasoning(canonical)) => {
                assert_eq!(
                    decoded.text(),
                    canonical.text(),
                    "item {index}: the reasoning"
                );
                assert_eq!(
                    decoded.signature(),
                    canonical.signature(),
                    "item {index}: the signature"
                );
            }
            _ => panic!("item {index} is a different kind: {decoded:?} vs {canonical:?}"),
        }
    }
}

/// Asserts a decoded response *is* the canonical answer, field by field.
pub(crate) fn assert_response_translates(decoded: &ChatResponse) {
    let canonical = canonical_response();
    assert_eq!(decoded.id(), canonical.id(), "the response id");
    assert_eq!(decoded.model(), canonical.model(), "the physical model");
    assert_eq!(
        decoded.choices().len(),
        canonical.choices().len(),
        "one candidate"
    );
    for (decoded, canonical) in decoded.choices().iter().zip(canonical.choices()) {
        assert_eq!(decoded.index(), canonical.index(), "the candidate index");
        assert_eq!(
            decoded.finish_reason(),
            canonical.finish_reason(),
            "the finish reason"
        );
        assert_turn(decoded.turn(), canonical.turn());
    }
    for kind in [TokenKind::Input, TokenKind::Output, TokenKind::CacheRead] {
        assert_eq!(
            decoded
                .usage()
                .reported_counts()
                .and_then(|counts| counts.get(kind)),
            canonical
                .usage()
                .reported_counts()
                .and_then(|counts| counts.get(kind)),
            "the reported {kind:?} count",
        );
    }
}

/// Asserts two turns carry the same reasoning, text and calls.
fn assert_turn(decoded: &AssistantTurn, canonical: &AssistantTurn) {
    assert_eq!(decoded.content(), canonical.content(), "the answer's parts");
    assert_eq!(decoded.tool_calls(), canonical.tool_calls(), "the calls");
    match (decoded.reasoning(), canonical.reasoning()) {
        (None, None) => {}
        (Some(decoded), Some(canonical)) => {
            assert_eq!(decoded.text(), canonical.text(), "the reasoning text");
            assert_eq!(
                decoded.signature(),
                canonical.signature(),
                "the reasoning signature"
            );
        }
        (decoded, canonical) => {
            panic!("the reasoning differs: {decoded:?} vs {canonical:?}")
        }
    }
}
