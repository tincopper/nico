//! The wire shape of one message, its content parts and the tool calls it carries.
//!
//! A wire message is not one canonical item. An assistant message can carry reasoning, visible text
//! and several tool calls at once, and a `role: "tool"` message is a tool *result* rather than a
//! message at all. The translation here is therefore one message to zero or more [`Item`]s, ordered
//! the way the model produced them: reasoning, then text, then the calls.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::{field, invalid, reject_extra, schema, unsupported, unsupported_capability};
use crate::{
    protocol::{Capability, CodecError},
    schema::{
        ContentPart, ImageSource, Item, Message, RawJson, Reasoning, Role, ToolCall, ToolCallId,
        ToolName, ToolResult,
    },
};

/// Decodes every message of a conversation, in order.
pub(super) fn decode(messages: Vec<MessageWire>) -> Result<Vec<Item>, CodecError> {
    let mut items = Vec::new();
    for (position, message) in messages.into_iter().enumerate() {
        message.decode(position, &mut items)?;
    }
    Ok(items)
}

/// One `messages[]` entry as the wire carries it.
#[derive(Deserialize)]
pub(super) struct MessageWire {
    role: String,
    #[serde(default)]
    content: Option<ContentWire>,
    #[serde(default)]
    tool_call_id: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallWire>>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    refusal: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

impl MessageWire {
    /// Appends the items one message produces.
    fn decode(self, position: usize, items: &mut Vec<Item>) -> Result<(), CodecError> {
        let path = format!("messages[{position}]");
        reject_extra(&self.extra, &path)?;
        self.reject_known(&path)?;
        self.reject_role_fields(&path)?;

        match self.role.as_str() {
            "system" | "developer" => {
                let role = if self.role == "system" {
                    Role::System
                } else {
                    Role::Developer
                };
                let parts = message_content(self.content, &path)?;
                items.push(Item::Message(
                    Message::try_new(role, parts).map_err(schema)?,
                ));
            }
            "user" => {
                let parts = message_content(self.content, &path)?;
                items.push(Item::Message(
                    Message::try_new(Role::User, parts).map_err(schema)?,
                ));
            }
            "assistant" => self.decode_assistant(&path, items)?,
            "tool" => self.decode_tool_result(&path, items)?,
            "function" => return Err(unsupported(format!("{path}.role"))),
            _ => {
                return Err(invalid(format!(
                    "field `{path}.role` is not a role this gateway understands"
                )));
            }
        }
        Ok(())
    }

    /// Refuses the known message fields whose semantics the canonical model does not carry.
    fn reject_known(&self, path: &str) -> Result<(), CodecError> {
        if self.name.is_some() {
            return Err(unsupported(format!("{path}.name")));
        }
        if self.refusal.is_some() {
            return Err(unsupported(format!("{path}.refusal")));
        }
        Ok(())
    }

    /// Refuses a member that belongs to another role, so nothing is dropped for lack of a home.
    fn reject_role_fields(&self, path: &str) -> Result<(), CodecError> {
        if self.role != "assistant" && self.tool_calls.is_some() {
            return Err(invalid(format!(
                "field `{path}.tool_calls` is only valid on an assistant message"
            )));
        }
        if self.role != "assistant" && self.reasoning_content.is_some() {
            return Err(invalid(format!(
                "field `{path}.reasoning_content` is only valid on an assistant message"
            )));
        }
        if self.role != "tool" && self.tool_call_id.is_some() {
            return Err(invalid(format!(
                "field `{path}.tool_call_id` is only valid on a tool result"
            )));
        }
        Ok(())
    }

    /// Turns an assistant message into reasoning, text and tool-call items.
    fn decode_assistant(self, path: &str, items: &mut Vec<Item>) -> Result<(), CodecError> {
        if let Some(reasoning) = self.reasoning_content {
            // A provider that streams reasoning sends an empty field on the turns that carry none.
            if !reasoning.trim().is_empty() {
                items.push(Item::Reasoning(
                    Reasoning::try_new(reasoning, None).map_err(schema)?,
                ));
            }
        }
        let parts = message_content(self.content, path)?;
        let calls = self
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(index, call)| call.decode(&format!("{path}.tool_calls[{index}]")))
            .collect::<Result<Vec<_>, _>>()?;

        if parts.is_empty() && calls.is_empty() {
            return Err(invalid(format!(
                "field `{path}` carries neither content nor a tool call"
            )));
        }
        if !parts.is_empty() {
            items.push(Item::Message(
                Message::try_new(Role::Assistant, parts).map_err(schema)?,
            ));
        }
        items.extend(calls.into_iter().map(Item::ToolCall));
        Ok(())
    }

    /// Turns a `role: "tool"` message into a tool-result item.
    fn decode_tool_result(self, path: &str, items: &mut Vec<Item>) -> Result<(), CodecError> {
        if self.tool_calls.is_some() {
            return Err(invalid(format!(
                "field `{path}.tool_calls` is not valid on a tool result"
            )));
        }
        let Some(id) = self.tool_call_id else {
            return Err(invalid(format!(
                "field `{path}.tool_call_id` is required on a tool result"
            )));
        };
        let id = ToolCallId::try_new(id)
            .map_err(|error| field(&format!("{path}.tool_call_id"), error))?;
        let parts = message_content(self.content, path)?;
        items.push(Item::ToolResult(
            ToolResult::try_new(id, parts).map_err(schema)?,
        ));
        Ok(())
    }
}

/// One `content` value: a plain string, or an array of typed parts.
#[derive(Deserialize)]
#[serde(untagged)]
enum ContentWire {
    Text(String),
    Parts(Vec<PartWire>),
}

/// One part of an array-shaped content.
#[derive(Deserialize)]
struct PartWire {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    image_url: Option<ImageUrlWire>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

/// One remote image reference.
#[derive(Deserialize)]
struct ImageUrlWire {
    url: String,
    #[serde(default)]
    detail: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

/// One tool call the assistant produced earlier in the conversation.
#[derive(Deserialize)]
struct ToolCallWire {
    id: String,
    #[serde(rename = "type")]
    kind: Option<String>,
    function: FunctionCallWire,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

/// The call's name and its arguments, which the wire carries as a JSON *string*.
#[derive(Deserialize)]
struct FunctionCallWire {
    name: String,
    arguments: String,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

impl ToolCallWire {
    /// Decodes one call.
    fn decode(self, path: &str) -> Result<ToolCall, CodecError> {
        reject_extra(&self.extra, path)?;
        if !matches!(self.kind.as_deref(), None | Some("function")) {
            return Err(unsupported(format!("{path}.type")));
        }
        reject_extra(&self.function.extra, &format!("{path}.function"))?;
        let name = ToolName::try_new(self.function.name)
            .map_err(|error| field(&format!("{path}.function.name"), error))?;
        let arguments = RawJson::parse(&self.function.arguments)
            .map_err(|error| field(&format!("{path}.function.arguments"), error))?;
        ToolCall::try_new(
            ToolCallId::try_new(self.id).map_err(|error| field(&format!("{path}.id"), error))?,
            name,
            arguments,
        )
        .map_err(schema)
    }
}

/// Decodes a message's content, refusing a part the canonical model cannot carry.
fn message_content(
    content: Option<ContentWire>,
    path: &str,
) -> Result<Vec<ContentPart>, CodecError> {
    let parts = match content {
        None => Vec::new(),
        Some(ContentWire::Text(text)) => vec![ContentPart::text(text)],
        Some(ContentWire::Parts(parts)) => parts
            .into_iter()
            .enumerate()
            .map(|(index, part)| part.decode(&format!("{path}.content[{index}]")))
            .collect::<Result<Vec<_>, _>>()?,
    };
    Ok(parts)
}

impl PartWire {
    /// Decodes one content part.
    fn decode(self, path: &str) -> Result<ContentPart, CodecError> {
        match self.kind.as_str() {
            "text" => {
                let text = self
                    .text
                    .ok_or_else(|| invalid(format!("field `{path}.text` is required")))?;
                if self.image_url.is_some() {
                    return Err(invalid(format!(
                        "field `{path}.image_url` is not valid on a text part"
                    )));
                }
                reject_extra(&self.extra, path)?;
                Ok(ContentPart::text(text))
            }
            "image_url" => {
                let image = self
                    .image_url
                    .ok_or_else(|| invalid(format!("field `{path}.image_url` is required")))?;
                if self.text.is_some() {
                    return Err(invalid(format!(
                        "field `{path}.text` is not valid on an image part"
                    )));
                }
                reject_extra(&self.extra, path)?;
                image.decode(path)
            }
            "input_audio" => Err(unsupported(path.to_owned())),
            "file" => Err(unsupported_capability(Capability::Files)),
            _ => Err(invalid(format!(
                "field `{path}.type` is not a content part this gateway understands"
            ))),
        }
    }
}

impl ImageUrlWire {
    /// Decodes one image reference.
    fn decode(self, path: &str) -> Result<ContentPart, CodecError> {
        let value = format!("{path}.image_url");
        reject_extra(&self.extra, &value)?;
        if self.detail.is_some() {
            return Err(unsupported(format!("{value}.detail")));
        }
        if self.url.trim().is_empty() {
            return Err(invalid(format!("field `{value}.url` is required")));
        }
        // An inline `data:` URL carries the bytes base64-encoded; decoding it needs a codec this
        // step does not have, so it is refused instead of being forwarded as an unvalidated URL.
        if self
            .url
            .get(..5)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
        {
            return Err(unsupported(format!("{value}.url")));
        }
        Ok(ContentPart::Image(ImageSource::Url(self.url)))
    }
}
