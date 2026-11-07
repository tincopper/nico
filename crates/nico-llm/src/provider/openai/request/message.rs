//! The conversation as `messages[]`: folding assistant items into one entry and encoding content.
//!
//! The canonical conversation keeps reasoning, visible text and tool calls as separate items; this
//! protocol carries them in one assistant message. A run of assistant items is therefore accumulated
//! and flushed when an item of another role arrives, which preserves the canonical order while
//! producing the wire's one-entry-per-turn shape.
//!
//! Nothing is dropped. A shape this document cannot carry is refused with a typed error, never
//! flattened into visible text: a file is an undeclared capability, and a signed reasoning block, a
//! failed tool result, and an image outside a user turn name the canonical field they carry. The
//! first is `Unsupported`, the rest are `UnsupportedField`; both answer `422` and neither quotes the
//! value.

use serde::Serialize;

use super::super::{unsupported, unsupported_field};
use crate::{
    protocol::Capability,
    provider::ProviderError,
    schema::{
        ChatRequest, ContentPart, ImageSource, Item, Message, Reasoning, Role, ToolCall, ToolResult,
    },
    wire::base64,
};

/// One conversation entry.
#[derive(Serialize)]
pub(super) struct MessageWire<'a> {
    role: &'static str,
    /// `null` for a turn that only called tools; the protocol spells that absence explicitly.
    content: Option<ContentWire>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCallWire<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
}

/// A message's content: the common string form, or the array a message with an image needs.
#[derive(Serialize)]
#[serde(untagged)]
enum ContentWire {
    Text(String),
    Parts(Vec<PartWire>),
}

/// One typed content part.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PartWire {
    Text { text: String },
    ImageUrl { image_url: ImageUrlWire },
}

/// A remote image address, which may be a `data:` URL for inline bytes.
#[derive(Serialize)]
struct ImageUrlWire {
    url: String,
}

/// One call the model asked the caller to make; the protocol carries the arguments as a JSON string.
#[derive(Serialize)]
struct ToolCallWire<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    function: FunctionCallWire<'a>,
}

/// A call's name and arguments.
#[derive(Serialize)]
struct FunctionCallWire<'a> {
    name: &'a str,
    arguments: String,
}

/// A run of assistant items that one wire message carries.
#[derive(Default)]
struct AssistantWire<'a> {
    reasoning: Option<String>,
    parts: Vec<&'a ContentPart>,
    calls: Vec<&'a ToolCall>,
}

impl<'a> AssistantWire<'a> {
    /// Folds one reasoning item into the run, refusing one this document cannot send back.
    fn push_reasoning(&mut self, reasoning: &'a Reasoning) -> Result<(), ProviderError> {
        if reasoning.signature().is_some() {
            return Err(unsupported_field("reasoning.signature"));
        }
        match &mut self.reasoning {
            Some(text) => text.push_str(reasoning.text()),
            None => self.reasoning = Some(reasoning.text().to_owned()),
        }
        Ok(())
    }

    /// Folds one assistant message's content into the run.
    fn push_message(&mut self, message: &'a Message) {
        self.parts.extend(message.content());
    }
}

/// Builds one wire message per conversation entry, folding an assistant run into one.
pub(super) fn messages<'a>(
    request: &'a ChatRequest,
) -> Result<Vec<MessageWire<'a>>, ProviderError> {
    let mut messages = Vec::new();
    let mut assistant: Option<AssistantWire<'a>> = None;
    for item in request.conversation() {
        match item {
            Item::Reasoning(reasoning) => assistant
                .get_or_insert_with(AssistantWire::default)
                .push_reasoning(reasoning)?,
            Item::Message(message) if message.role() == Role::Assistant => assistant
                .get_or_insert_with(AssistantWire::default)
                .push_message(message),
            Item::ToolCall(call) => assistant
                .get_or_insert_with(AssistantWire::default)
                .calls
                .push(call),
            Item::Message(message) => {
                flush(&mut messages, &mut assistant)?;
                messages.push(message_wire(message)?);
            }
            Item::ToolResult(result) => {
                flush(&mut messages, &mut assistant)?;
                messages.push(tool_result_wire(result)?);
            }
        }
    }
    flush(&mut messages, &mut assistant)?;
    Ok(messages)
}

/// Writes the assistant run that ended.
fn flush<'a>(
    messages: &mut Vec<MessageWire<'a>>,
    assistant: &mut Option<AssistantWire<'a>>,
) -> Result<(), ProviderError> {
    let Some(builder) = assistant.take() else {
        return Ok(());
    };
    let content = if builder.parts.is_empty() {
        None
    } else {
        Some(ContentWire::Text(joined_text(
            builder.parts.iter().copied(),
            "message.content",
        )?))
    };
    let has_calls = !builder.calls.is_empty();
    let calls = builder.calls.into_iter().map(call_wire).collect();
    messages.push(MessageWire {
        role: "assistant",
        content,
        tool_call_id: None,
        tool_calls: if has_calls { Some(calls) } else { None },
        reasoning_content: builder.reasoning,
    });
    Ok(())
}

/// Builds one non-assistant message.
fn message_wire(message: &Message) -> Result<MessageWire<'_>, ProviderError> {
    let role = match message.role() {
        Role::System => "system",
        Role::Developer => "developer",
        Role::User => "user",
        // An assistant message never reaches this function: it is folded into the run instead.
        Role::Assistant => "assistant",
    };
    let content = message_content(message.role(), message.content())?;
    Ok(MessageWire {
        role,
        content: Some(content),
        tool_call_id: None,
        tool_calls: None,
        reasoning_content: None,
    })
}

/// Builds one tool-result message.
fn tool_result_wire(result: &ToolResult) -> Result<MessageWire<'_>, ProviderError> {
    if result.is_error() {
        return Err(unsupported_field("tool_result.is_error"));
    }
    let content = joined_text(result.content(), "tool_result.content")?;
    Ok(MessageWire {
        role: "tool",
        content: Some(ContentWire::Text(content)),
        tool_call_id: Some(result.id().as_str()),
        tool_calls: None,
        reasoning_content: None,
    })
}

/// Builds a message's content.
fn message_content(role: Role, parts: &[ContentPart]) -> Result<ContentWire, ProviderError> {
    if role == Role::User
        && parts
            .iter()
            .any(|part| matches!(part, ContentPart::Image(_)))
    {
        let mut wires = Vec::with_capacity(parts.len());
        for part in parts {
            wires.push(match part {
                ContentPart::Text(text) => PartWire::Text { text: text.clone() },
                ContentPart::Image(source) => PartWire::ImageUrl {
                    image_url: ImageUrlWire {
                        url: image_url(source),
                    },
                },
                ContentPart::File { .. } => return Err(unsupported(Capability::Files)),
            });
        }
        return Ok(ContentWire::Parts(wires));
    }
    Ok(ContentWire::Text(joined_text(parts, "message.content")?))
}

/// Joins the visible text of a part list, refusing a part that has no place in a string field.
///
/// `field` is the canonical path the refusal names: embedding an image in visible text would change
/// what the caller asked for, so the position is reported instead of being approximated.
fn joined_text<'a>(
    parts: impl IntoIterator<Item = &'a ContentPart>,
    field: &'static str,
) -> Result<String, ProviderError> {
    let mut text = String::new();
    for part in parts {
        match part {
            ContentPart::Text(fragment) => text.push_str(fragment),
            ContentPart::Image(_) => return Err(unsupported_field(field)),
            ContentPart::File { .. } => return Err(unsupported(Capability::Files)),
        }
    }
    Ok(text)
}

/// Returns the URL an image part is sent as.
fn image_url(source: &ImageSource) -> String {
    match source {
        ImageSource::Url(url) => url.clone(),
        ImageSource::Base64 { media_type, data } => {
            format!("data:{media_type};base64,{}", base64::encode(data))
        }
    }
}

/// Builds one tool call, encoding its arguments as the JSON string the protocol carries.
fn call_wire(call: &ToolCall) -> ToolCallWire<'_> {
    ToolCallWire {
        id: call.id().as_str(),
        kind: "function",
        function: FunctionCallWire {
            name: call.name().as_str(),
            arguments: call.arguments().to_text(),
        },
    }
}
