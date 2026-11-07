//! The content blocks of one message: decoding them into canonical items, and writing them back.
//!
//! A message's content is a string in the simplest case and a block array otherwise, and the blocks
//! are where this protocol differs most from the canonical model: a tool call, a tool result and a
//! thinking block are *content* here and first-class items there. Decoding therefore returns a
//! [`Decoded`] per block and lets the caller keep the wire order: text runs become one message, and a
//! tool/reasoning block flushes the run before it.
//!
//! # Refusals
//!
//! A block type this build does not map is named and refused, never skipped: a block carries content
//! the caller asked to send, and dropping one changes the conversation. Known types with no canonical
//! home are [`CodecError::UnsupportedField`] (`redacted_thinking`, `document`, the server-tool
//! blocks); an unknown type is a document this protocol does not have, so it is
//! [`CodecError::InvalidRequest`].

use serde::{Deserialize, Serialize};

use super::{PROTOCOL, field, invalid, schema, unsupported};
use crate::{
    protocol::CodecError,
    schema::{
        AssistantTurn, ContentPart, ImageSource, Item, RawJson, Reasoning, ToolCall, ToolCallId,
        ToolName, ToolResult,
    },
    wire::base64,
};

/// One block of a message after decoding.
pub(super) enum Decoded {
    /// A part of the visible (or image) content of a message.
    Part(ContentPart),
    /// A canonical item that is not message content: a call, a result or a reasoning block.
    Item(Item),
}

/// One block of an answer, ready to serialize.
///
/// The array is always written, even for a single text block: one spelling keeps a golden fixture and
/// a streamed event from having two forms of the same answer.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum BlockOut<'a> {
    /// Visible text.
    ///
    /// Owned rather than borrowed: a turn may carry several text parts that the protocol writes as
    /// one block, so the joined value has no caller-owned source to point at.
    Text {
        /// The text the model produced.
        text: String,
    },
    /// Hidden reasoning, with the provider signature when the provider issued one.
    Thinking {
        /// The reasoning text.
        thinking: &'a str,
        /// The opaque signature, written only when the canonical value carries one.
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<&'a str>,
    },
    /// A call the model asked the caller to make.
    ToolUse {
        /// The call's identifier.
        id: &'a str,
        /// The tool being called.
        name: &'a str,
        /// The call's arguments, as a JSON object.
        input: &'a RawJson,
    },
}

/// Decodes one block, keeping the wire order in the caller's hands.
pub(super) fn decode_block(raw: &RawJson, path: &str) -> Result<Decoded, CodecError> {
    let kind = raw
        .as_value()
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid(format!("`{path}` is a block without a `type`")))?;
    match kind {
        "text" | "image" | "tool_use" | "tool_result" | "thinking" => {
            decode_known(decode::<BlockIn>(raw, path)?, path)
        }
        // A signed block whose text the gateway is not allowed to see. The signature belongs to the
        // provider that issued it; keeping the blob without its text would send a conversation the
        // model cannot reason about, and dropping it would silently shorten the history.
        "redacted_thinking" => Err(unsupported(path.to_owned())),
        // Documents are addressed by media type and bytes; the canonical file part exists but no
        // adapter in this build carries inline bytes, so the field is refused rather than dropped.
        "document" => Err(unsupported(path.to_owned())),
        // Server-side tools run at the provider; the gateway neither issued the call nor can replay
        // its result. A route that needs them is a route this build does not serve.
        "server_tool_use"
        | "web_search_tool_result"
        | "code_execution_tool_result"
        | "bash_tool_result"
        | "text_editor_tool_result" => Err(unsupported(path.to_owned())),
        other => Err(invalid(format!(
            "`{path}` is a block type this protocol does not define: `{}`",
            diagnostic_name(other)
        ))),
    }
}

/// Turns one typed block into a canonical value.
fn decode_known(block: BlockIn, path: &str) -> Result<Decoded, CodecError> {
    match block {
        BlockIn::Text { text } => Ok(Decoded::Part(ContentPart::Text(text))),
        BlockIn::Image { source } => Ok(Decoded::Part(ContentPart::Image(image_source(
            source,
            &format!("{path}.source"),
        )?))),
        BlockIn::ToolUse { id, name, input } => {
            let call = ToolCall::try_new(
                ToolCallId::try_new(id).map_err(|error| field(&format!("{path}.id"), error))?,
                ToolName::try_new(name).map_err(|error| field(&format!("{path}.name"), error))?,
                input,
            )
            .map_err(schema)?;
            Ok(Decoded::Item(Item::ToolCall(call)))
        }
        BlockIn::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            let parts = tool_result_parts(content.as_ref(), &format!("{path}.content"))?;
            let result = ToolResult::try_new(
                ToolCallId::try_new(tool_use_id)
                    .map_err(|error| field(&format!("{path}.tool_use_id"), error))?,
                parts,
            )
            .map_err(schema)?
            .with_error(is_error);
            Ok(Decoded::Item(Item::ToolResult(result)))
        }
        BlockIn::Thinking {
            thinking,
            signature,
        } => {
            let reasoning =
                Reasoning::try_new(thinking, signature).map_err(|error| field(path, error))?;
            Ok(Decoded::Item(Item::Reasoning(reasoning)))
        }
    }
}

/// Decodes the `system` field: a string, or an array of text blocks.
pub(super) fn system_parts(system: &RawJson, path: &str) -> Result<Vec<ContentPart>, CodecError> {
    match system.as_value() {
        serde_json::Value::String(text) => {
            if text.is_empty() {
                return Err(invalid(format!("`{path}` is an empty string")));
            }
            Ok(vec![ContentPart::text(text.clone())])
        }
        serde_json::Value::Array(_) => {
            let parts = text_parts(system, path)?;
            if parts.is_empty() {
                return Err(invalid(format!("`{path}` carries no text")));
            }
            Ok(parts)
        }
        _ => Err(invalid(format!(
            "`{path}` must be a string or an array of text blocks"
        ))),
    }
}

/// Decodes the content of one message into its parts and items, in wire order.
///
/// `parts` is the caller's message assembly: a run of text/image blocks becomes one message, so the
/// caller supplies the role and decides when the run is flushed.
pub(super) fn decode_content(
    content: &RawJson,
    path: &str,
    decode: impl FnMut(Decoded),
) -> Result<(), CodecError> {
    let mut decode = decode;
    match content.as_value() {
        serde_json::Value::String(text) => {
            decode(Decoded::Part(ContentPart::Text(text.clone())));
            Ok(())
        }
        serde_json::Value::Array(blocks) => {
            for (index, block) in blocks.iter().enumerate() {
                let raw = RawJson::new(block.clone());
                decode(decode_block(&raw, &format!("{path}[{index}]"))?);
            }
            Ok(())
        }
        _ => Err(invalid(format!(
            "`{path}` must be a string or an array of blocks"
        ))),
    }
}

/// Decodes the content of a `tool_result`: text and images only.
pub(super) fn tool_result_parts(
    content: Option<&RawJson>,
    path: &str,
) -> Result<Vec<ContentPart>, CodecError> {
    let Some(content) = content else {
        // The protocol allows a result with nothing in it; the canonical model requires at least one
        // part, so "the tool said nothing" is one empty text part rather than an invented sentence.
        return Ok(vec![ContentPart::Text(String::new())]);
    };
    match content.as_value() {
        serde_json::Value::String(text) => Ok(vec![ContentPart::Text(text.clone())]),
        serde_json::Value::Array(blocks) => {
            let mut parts = Vec::with_capacity(blocks.len());
            for (index, block) in blocks.iter().enumerate() {
                let path = format!("{path}[{index}]");
                match decode_block(&RawJson::new(block.clone()), &path)? {
                    Decoded::Part(part) => parts.push(part),
                    Decoded::Item(_) => {
                        return Err(invalid(format!(
                            "`{path}` is not a content part of a tool result"
                        )));
                    }
                }
            }
            if parts.is_empty() {
                return Ok(vec![ContentPart::Text(String::new())]);
            }
            Ok(parts)
        }
        _ => Err(invalid(format!(
            "`{path}` must be a string or an array of blocks"
        ))),
    }
}

/// Writes one assistant turn as output blocks, in the order the protocol expects.
///
/// Reasoning comes first because a thinking block must precede the text it produced. A turn with no
/// visible text writes no text block: an empty one would claim the model produced an empty answer.
pub(super) fn assistant_blocks(turn: &AssistantTurn) -> Result<Vec<BlockOut<'_>>, CodecError> {
    let mut blocks = Vec::with_capacity(turn.content().len() + turn.tool_calls().len() + 1);
    if let Some(reasoning) = turn.reasoning() {
        blocks.push(BlockOut::Thinking {
            thinking: reasoning.text(),
            signature: reasoning.signature(),
        });
    }
    let mut text = String::new();
    let mut has_text = false;
    for part in turn.content() {
        match part {
            ContentPart::Text(fragment) => {
                has_text = true;
                text.push_str(fragment);
            }
            ContentPart::Image(_) | ContentPart::File { .. } => {
                return Err(CodecError::Encode {
                    protocol: PROTOCOL,
                    reason: "the turn carries a content part Anthropic Messages cannot express"
                        .to_owned(),
                });
            }
        }
    }
    if has_text {
        blocks.push(BlockOut::Text { text });
    }
    for call in turn.tool_calls() {
        blocks.push(BlockOut::ToolUse {
            id: call.id().as_str(),
            name: call.name().as_str(),
            input: call.arguments(),
        });
    }
    Ok(blocks)
}

/// Decodes one typed block, reporting the field path rather than a parse position.
///
/// `serde`'s own message may quote a value; it stays in the codec diagnostic, which only ever reaches
/// the log, and the client reads the canonical error's constant message instead.
fn decode<T: serde::de::DeserializeOwned>(raw: &RawJson, path: &str) -> Result<T, CodecError> {
    serde_json::from_value(raw.as_value().clone())
        .map_err(|error| invalid(format!("field `{path}` is invalid: {error}")))
}

/// Decodes an image source: inline base64 bytes or a remote URL.
fn image_source(source: ImageSourceWire, path: &str) -> Result<ImageSource, CodecError> {
    match source {
        ImageSourceWire::Base64 {
            media_type, data, ..
        } => {
            let bytes = base64::decode(&data)
                .map_err(|error| invalid(format!("`{path}.data` is not valid base64: {error}")))?;
            Ok(ImageSource::Base64 {
                media_type,
                data: bytes,
            })
        }
        ImageSourceWire::Url { url } => Ok(ImageSource::Url(url)),
    }
}

/// Decodes the text blocks of a `system` array.
fn text_parts(system: &RawJson, path: &str) -> Result<Vec<ContentPart>, CodecError> {
    let blocks = system
        .as_value()
        .as_array()
        .ok_or_else(|| invalid(format!("`{path}` must be an array of text blocks")))?;
    let mut parts = Vec::with_capacity(blocks.len());
    for (index, block) in blocks.iter().enumerate() {
        let path = format!("{path}[{index}]");
        let raw = RawJson::new(block.clone());
        match decode_block(&raw, &path)? {
            Decoded::Part(ContentPart::Text(text)) => parts.push(ContentPart::Text(text)),
            Decoded::Part(_) | Decoded::Item(_) => {
                return Err(invalid(format!("`{path}` is not a text block")));
            }
        }
    }
    Ok(parts)
}

/// Prints a block type this codec does not know, or describes it when the name cannot be printed.
fn diagnostic_name(name: &str) -> &str {
    crate::diagnostic::quoted(name, crate::diagnostic::MAX_QUOTED_FIELD_BYTES)
        .unwrap_or("<unnamed>")
}

/// A block this codec models.
///
/// The enum is the closed set: a block type that is not one of these variants is refused by
/// [`decode_block`] before this type is reached, so a missing variant cannot become a silent skip.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum BlockIn {
    /// Visible text.
    Text { text: String },
    /// An image the model reads.
    Image { source: ImageSourceWire },
    /// A call the model asked the caller to make.
    ToolUse {
        id: String,
        name: String,
        input: RawJson,
    },
    /// A result the caller produced.
    ToolResult {
        tool_use_id: String,
        #[serde(default)]
        content: Option<RawJson>,
        #[serde(default)]
        is_error: bool,
    },
    /// Hidden reasoning, signed by the provider when it issued one.
    Thinking {
        thinking: String,
        #[serde(default)]
        signature: Option<String>,
    },
}

/// Where an image's bytes live.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum ImageSourceWire {
    /// Inline base64 bytes with their media type.
    Base64 { media_type: String, data: String },
    /// A remote URL the provider fetches.
    Url { url: String },
}
