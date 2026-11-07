//! Decoding a Messages response document into canonical values.
//!
//! A success body is read into a [`ChatResponse`]; a failure status never reaches this module's parser
//! because [`super::error`] maps it first, without decoding the provider's message. What this document
//! carries and the canonical model does not:
//!
//! - `type`, `role` and `stop_sequence` describe the provider's framing and are consumed rather than
//!   preserved. `stop_sequence` is the caller's own stop string echoed back; the canonical model keeps
//!   the *reason* (`Stop`), and writing the string into another protocol's answer would be a field
//!   that means nothing there.
//! - An unknown *top-level* field is preserved in [`Extensions`] and written back by the client codec.
//! - An unknown member inside a content block is refused: a block is what the caller reads, and a
//!   member this adapter does not model could change it. `citations` is accepted only when it is
//!   null, because a citation is produced by a request feature this adapter refuses.
//!
//! The three block kinds become one [`AssistantTurn`]: `text` blocks are the visible content,
//! `tool_use` blocks are the calls, and a `thinking` block is the reasoning (its `signature` carried
//! verbatim). A block type this adapter does not know is [`ProviderError::Malformed`]: skipping it
//! would hide part of the answer.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::{
    finish_reason, malformed, parse, reject_extra, schema_failure,
    usage::{self, UsageWire},
};
use crate::{
    provider::{ProviderError, ResponseFacts},
    schema::{
        AssistantTurn, ChatResponse, Choice, ContentPart, Extensions, ModelId, RawJson, Reasoning,
        ResponseId, ToolCall, ToolCallId, ToolName,
    },
};

/// The `type` value a complete answer carries.
const MESSAGE_OBJECT: &str = "message";

/// Decodes one provider response.
///
/// # Errors
/// Returns [`ProviderError::Upstream`] for a failure status, and [`ProviderError::Malformed`] when a
/// success body is not a complete message this adapter can express.
pub(super) fn decode(head: &ResponseFacts, body: &[u8]) -> Result<ChatResponse, ProviderError> {
    if !head.is_success() {
        return Err(ProviderError::Upstream {
            error: super::error::failure(head.status(), body),
        });
    }
    let wire: ResponseWire = parse(body, "the response body")?;
    wire.into_response()
}

/// A complete message answer.
#[derive(Deserialize)]
struct ResponseWire {
    id: String,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    role: Option<String>,
    model: String,
    content: Vec<BlockWire>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default, rename = "stop_sequence")]
    _stop_sequence: Option<RawJson>,
    #[serde(default)]
    usage: Option<UsageWire>,
    #[serde(flatten)]
    extensions: Extensions,
}

impl ResponseWire {
    /// Builds the canonical response.
    fn into_response(self) -> Result<ChatResponse, ProviderError> {
        if let Some(kind) = &self.kind
            && kind != MESSAGE_OBJECT
        {
            return Err(malformed(format!(
                "the document is not a complete message (its `type` is not `{MESSAGE_OBJECT}`)"
            )));
        }
        if let Some(role) = &self.role
            && role != "assistant"
        {
            return Err(malformed("the answer's message is not an assistant turn"));
        }
        let id = ResponseId::try_new(self.id).map_err(schema_failure)?;
        let model = ModelId::try_new(self.model).map_err(schema_failure)?;
        // A complete answer always reports why generation stopped; an absent reason would make the
        // caller guess, which is what the stream decoder also refuses to do.
        let reason = self
            .stop_reason
            .as_deref()
            .ok_or_else(|| malformed("the answer reports no stop reason"))?;
        let finish = finish_reason(reason)?;
        let mut turn = TurnBuilder::default();
        for block in self.content {
            turn.push(block.into_block()?)?;
        }
        let turn = turn.build()?;
        let choice = Choice::try_new(0, turn, finish).map_err(schema_failure)?;
        let mut response =
            ChatResponse::try_new(id, model, vec![choice]).map_err(schema_failure)?;
        if let Some(usage) = &self.usage {
            response = response.with_usage(usage::decode(usage));
        }
        response = response.with_extensions(self.extensions);
        response.validate().map_err(schema_failure)?;
        Ok(response)
    }
}

/// One assistant turn under construction.
#[derive(Default)]
struct TurnBuilder {
    content: Vec<ContentPart>,
    calls: Vec<ToolCall>,
    reasoning: Option<Reasoning>,
}

impl TurnBuilder {
    /// Folds one decoded block into the turn.
    fn push(&mut self, block: Block) -> Result<(), ProviderError> {
        match block {
            Block::Text(text) => self.content.push(ContentPart::text(text)),
            Block::Call(call) => self.calls.push(call),
            Block::Reasoning(reasoning) => {
                // The canonical turn carries one reasoning value, and the protocol sends one
                // thinking block per turn; a second would be silently merged into the first, which
                // is exactly the hidden change this adapter refuses elsewhere.
                if self.reasoning.is_some() {
                    return Err(malformed("the answer carries more than one thinking block"));
                }
                self.reasoning = Some(reasoning);
            }
        }
        Ok(())
    }

    /// Builds the turn.
    fn build(self) -> Result<AssistantTurn, ProviderError> {
        let turn = AssistantTurn::try_new(self.content, self.calls).map_err(schema_failure)?;
        Ok(match self.reasoning {
            Some(reasoning) => turn.with_reasoning(reasoning),
            None => turn,
        })
    }
}

/// One decoded content block.
enum Block {
    Text(String),
    Call(ToolCall),
    Reasoning(Reasoning),
}

/// One content block as the wire spells it.
///
/// The block kinds share one struct instead of an enum because the protocol tags them with a string
/// member rather than by shape: an untagged enum would let a `text` block with an `id` decode as a
/// tool call. Every member is optional here and the `type` decides which are required.
#[derive(Deserialize)]
struct BlockWire {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    signature: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    input: Option<RawJson>,
    #[serde(default)]
    citations: Option<RawJson>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

impl BlockWire {
    /// Builds one canonical block.
    fn into_block(self) -> Result<Block, ProviderError> {
        reject_extra(&self.extra, "content[]")?;
        match self.kind.as_str() {
            "text" => {
                if let Some(citations) = &self.citations
                    && !citations.as_value().is_null()
                {
                    return Err(malformed(
                        "the answer carries citations, which this adapter cannot express",
                    ));
                }
                let text = self
                    .text
                    .ok_or_else(|| malformed("a content block of type `text` carries no text"))?;
                // An empty text block says nothing; the canonical model has no zero-length text part
                // that would mean the same, so it is not written as one.
                if text.is_empty() {
                    return Err(malformed("the answer carries an empty text block"));
                }
                Ok(Block::Text(text))
            }
            "tool_use" => {
                let id = self
                    .id
                    .ok_or_else(|| malformed("a tool call carries no id"))?;
                let name = self
                    .name
                    .ok_or_else(|| malformed("a tool call carries no name"))?;
                let input = self
                    .input
                    .ok_or_else(|| malformed("a tool call carries no arguments"))?;
                Ok(Block::Call(
                    ToolCall::try_new(
                        ToolCallId::try_new(id).map_err(schema_failure)?,
                        ToolName::try_new(name).map_err(schema_failure)?,
                        input,
                    )
                    .map_err(schema_failure)?,
                ))
            }
            "thinking" => {
                let text = self
                    .thinking
                    .ok_or_else(|| malformed("a thinking block carries no text"))?;
                // An empty signature is the absence of one: the protocol leaves the field out when
                // the block was produced without one.
                let signature = self.signature.filter(|value| !value.is_empty());
                Ok(Block::Reasoning(
                    Reasoning::try_new(text, signature).map_err(schema_failure)?,
                ))
            }
            // `redacted_thinking` travels as opaque bytes with no canonical field; a block type this
            // adapter does not know could carry content the caller would otherwise see.
            _ => Err(malformed(
                "the answer carries a content block this adapter cannot express",
            )),
        }
    }
}
