//! The items of this protocol: the `input` array a caller sends and the `output` array it reads.
//!
//! An item is a message, a call, a result or a reasoning block — the same four things the canonical
//! [`Item`] names, in this protocol's spelling. Decoding therefore needs no folding: every wire item is
//! exactly one canonical item, which is why this codec has no "flush the run" logic of its own.
//!
//! # Refusals
//!
//! A `reasoning` item carries *two* replay handles (`id` and `encrypted_content`) and the canonical
//! model has one, so a request that sends one is refused as a missing capability rather than being
//! carried without the handle that makes it valid. An unknown item type is a document this protocol
//! does not have, so it is [`CodecError::InvalidRequest`].

use std::collections::BTreeMap;

use serde::Serialize;

use super::{field, invalid, reject_extra, schema, unsupported_capability};
use crate::{
    protocol::{Capability, CodecError},
    schema::{
        AssistantTurn, ContentPart, ImageSource, Item, Message, RawJson, Role, ToolCall,
        ToolCallId, ToolName, ToolResult,
    },
    wire::base64,
};

/// One item of the `output` array.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum OutputItem {
    /// An assistant message.
    Message {
        /// The item's identifier, which the streamed events also carry.
        id: String,
        /// Always `assistant`.
        role: &'static str,
        /// Always `completed`: a partial item is not written in a complete answer.
        status: &'static str,
        /// The text the model produced.
        content: Vec<OutputPart>,
    },
    /// Reasoning summary the provider reported.
    Reasoning {
        /// The item's identifier.
        id: String,
        /// The summary blocks this protocol renders reasoning as.
        summary: Vec<SummaryPart>,
    },
    /// A call the model asked the caller to make.
    FunctionCall {
        /// The item's identifier.
        id: String,
        /// The identifier the caller's answer must carry back.
        call_id: String,
        /// The tool being called.
        name: String,
        /// The arguments, as the JSON text the protocol carries.
        arguments: String,
        /// Always `completed`.
        status: &'static str,
    },
}

/// One part of an assistant message in the `output` array.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum OutputPart {
    /// Visible text.
    OutputText {
        /// The text.
        text: String,
        /// The protocol carries annotations; this gateway produces none, so the array is empty.
        annotations: Vec<RawJson>,
    },
}

/// One summary block of a reasoning item.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum SummaryPart {
    /// One paragraph of the summary.
    SummaryText {
        /// The text.
        text: String,
    },
}

/// Builds the reasoning item with `index`'s identifier.
pub(super) fn reasoning_item(index: usize, text: String) -> OutputItem {
    OutputItem::Reasoning {
        id: format!("item_{index}"),
        summary: vec![SummaryPart::SummaryText { text }],
    }
}

/// Builds the assistant message item with `index`'s identifier.
pub(super) fn message_item(index: usize, text: String) -> OutputItem {
    OutputItem::Message {
        id: format!("item_{index}"),
        role: "assistant",
        status: "completed",
        content: vec![OutputPart::OutputText {
            text,
            annotations: Vec::new(),
        }],
    }
}

/// Builds one function-call item with `index`'s identifier.
pub(super) fn call_item(index: usize, call_id: &str, name: &str, arguments: String) -> OutputItem {
    OutputItem::FunctionCall {
        id: format!("item_{index}"),
        call_id: call_id.to_owned(),
        name: name.to_owned(),
        arguments,
        status: "completed",
    }
}

/// Writes one assistant turn as the `output` array.
pub(super) fn from_turn(turn: &AssistantTurn) -> Result<Vec<OutputItem>, CodecError> {
    let mut items = Vec::with_capacity(turn.content().len() + turn.tool_calls().len() + 1);
    if let Some(reasoning) = turn.reasoning() {
        items.push(reasoning_item(0, reasoning.text().to_owned()));
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
                    protocol: super::PROTOCOL,
                    reason: "the turn carries a content part Responses cannot express".to_owned(),
                });
            }
        }
    }
    if has_text {
        items.push(message_item(items.len(), text));
    }
    for (offset, call) in turn.tool_calls().iter().enumerate() {
        items.push(call_item(
            items.len() + offset,
            call.id().as_str(),
            call.name().as_str(),
            call.arguments().to_text(),
        ));
    }
    Ok(items)
}

/// One item of the `input` array after decoding.
pub(super) enum Decoded {
    /// A message of the conversation.
    Message(Message),
    /// A call the model asked the caller to make.
    Item(Item),
}

/// Decodes one `input` item.
pub(super) fn decode_item(raw: &RawJson, path: &str) -> Result<Decoded, CodecError> {
    let kind = raw
        .as_value()
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid(format!("`{path}` is an item without a `type`")))?;
    match kind {
        "message" => {
            let item: MessageIn = decode(raw, path)?;
            reject_extra(&item.extra, path)?;
            let role = role(&item.role, path)?;
            let parts = parts(item.content.as_ref(), &format!("{path}.content"))?;
            Ok(Decoded::Message(
                Message::try_new(role, parts).map_err(schema)?,
            ))
        }
        "function_call" => {
            let item: CallIn = decode(raw, path)?;
            reject_extra(&item.extra, path)?;
            let call = ToolCall::try_new(
                ToolCallId::try_new(item.call_id)
                    .map_err(|error| field(&format!("{path}.call_id"), error))?,
                ToolName::try_new(item.name)
                    .map_err(|error| field(&format!("{path}.name"), error))?,
                RawJson::parse(&item.arguments)
                    .map_err(|error| field(&format!("{path}.arguments"), error))?,
            )
            .map_err(schema)?;
            Ok(Decoded::Item(Item::ToolCall(call)))
        }
        "function_call_output" => {
            let item: OutputIn = decode(raw, path)?;
            reject_extra(&item.extra, path)?;
            let result = ToolResult::try_new(
                ToolCallId::try_new(item.call_id)
                    .map_err(|error| field(&format!("{path}.call_id"), error))?,
                vec![ContentPart::Text(item.output)],
            )
            .map_err(schema)?;
            Ok(Decoded::Item(Item::ToolResult(result)))
        }
        // A reasoning item needs two replay handles and the canonical model carries one.
        "reasoning" => Err(unsupported_capability(Capability::Reasoning)),
        other => Err(invalid(format!(
            "`{path}` is an item type this protocol does not define: `{other}`"
        ))),
    }
}

/// Returns the canonical role of one wire role.
fn role(value: &str, path: &str) -> Result<Role, CodecError> {
    match value {
        "user" => Ok(Role::User),
        "assistant" => Ok(Role::Assistant),
        "system" => Ok(Role::System),
        "developer" => Ok(Role::Developer),
        other => Err(invalid(format!(
            "`{path}.role` is not a role this protocol defines: `{other}`"
        ))),
    }
}

/// Decodes the content of one message: a string, or an array of parts.
fn parts(content: Option<&RawJson>, path: &str) -> Result<Vec<ContentPart>, CodecError> {
    let Some(content) = content else {
        return Err(invalid(format!("`{path}` is missing")));
    };
    match content.as_value() {
        serde_json::Value::String(text) => Ok(vec![ContentPart::Text(text.clone())]),
        serde_json::Value::Array(raw) => {
            let mut parts = Vec::with_capacity(raw.len());
            for (index, part) in raw.iter().enumerate() {
                let path = format!("{path}[{index}]");
                parts.push(decode_part(&RawJson::new(part.clone()), &path)?);
            }
            if parts.is_empty() {
                return Err(invalid(format!("`{path}` carries no parts")));
            }
            Ok(parts)
        }
        _ => Err(invalid(format!(
            "`{path}` must be a string or an array of parts"
        ))),
    }
}

/// Decodes one part of a message.
fn decode_part(raw: &RawJson, path: &str) -> Result<ContentPart, CodecError> {
    let kind = raw
        .as_value()
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid(format!("`{path}` is a part without a `type`")))?;
    match kind {
        "input_text" | "output_text" => {
            let part: TextIn = decode(raw, path)?;
            reject_extra(&part.extra, path)?;
            Ok(ContentPart::Text(part.text))
        }
        "input_image" => {
            let part: ImageIn = decode(raw, path)?;
            reject_extra(&part.extra, path)?;
            image(&part.image_url, &format!("{path}.image_url"))
        }
        other => Err(invalid(format!(
            "`{path}` is a part type this protocol does not define in a message: `{other}`"
        ))),
    }
}

/// Decodes an image URL: a remote URL, or inline `data:` bytes.
fn image(url: &str, path: &str) -> Result<ContentPart, CodecError> {
    if url.starts_with("data:") {
        let (media_type, payload) = parse_data_url(url)
            .ok_or_else(|| invalid(format!("`{path}` is not a base64 data URL")))?;
        let data = base64::decode(payload)
            .map_err(|error| invalid(format!("`{path}` is not valid base64: {error}")))?;
        return Ok(ContentPart::Image(ImageSource::Base64 { media_type, data }));
    }
    Ok(ContentPart::Image(ImageSource::Url(url.to_owned())))
}

/// Splits `data:<media-type>;base64,<payload>`.
fn parse_data_url(url: &str) -> Option<(String, &str)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    let media_type = meta.strip_suffix(";base64")?;
    if media_type.is_empty() || !media_type.starts_with("image/") {
        return None;
    }
    Some((media_type.to_owned(), payload))
}

/// Decodes one typed item, reporting the field path rather than a parse position.
fn decode<T: serde::de::DeserializeOwned>(raw: &RawJson, path: &str) -> Result<T, CodecError> {
    serde_json::from_value(raw.as_value().clone())
        .map_err(|error| invalid(format!("field `{path}` is invalid: {error}")))
}

/// One message item.
#[derive(serde::Deserialize)]
struct MessageIn {
    /// The item's discriminator, which the dispatch above has already read.
    #[serde(rename = "type")]
    _kind: String,
    role: String,
    #[serde(default)]
    content: Option<RawJson>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

/// One call item.
#[derive(serde::Deserialize)]
struct CallIn {
    /// The item's discriminator, which the dispatch above has already read.
    #[serde(rename = "type")]
    _kind: String,
    call_id: String,
    name: String,
    arguments: String,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

/// One result item.
#[derive(serde::Deserialize)]
struct OutputIn {
    /// The item's discriminator, which the dispatch above has already read.
    #[serde(rename = "type")]
    _kind: String,
    call_id: String,
    output: String,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

/// One text part.
#[derive(serde::Deserialize)]
struct TextIn {
    /// The item's discriminator, which the dispatch above has already read.
    #[serde(rename = "type")]
    _kind: String,
    text: String,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

/// One image part.
#[derive(serde::Deserialize)]
struct ImageIn {
    /// The item's discriminator, which the dispatch above has already read.
    #[serde(rename = "type")]
    _kind: String,
    image_url: String,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}
