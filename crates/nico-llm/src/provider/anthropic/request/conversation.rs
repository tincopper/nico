//! The conversation as `messages[]`: folding runs, grouping tool results and encoding content blocks.
//!
//! The canonical conversation keeps text and tool calls as separate items while this protocol carries
//! them in one message, and a tool result belongs to a *user* turn even though the canonical item that
//! produced it follows an assistant one. The folding is therefore a small state machine: an assistant
//! run accumulates until another role arrives, and tool results accumulate until a user turn can carry
//! them at its head.
//!
//! Nothing is dropped. A shape this document cannot carry is refused with a typed error, never
//! flattened into visible text: a file is an undeclared capability, an image outside a user turn and a
//! reasoning item name the canonical field they carry. All of them answer `422`; none quotes the value.

use serde::Serialize;

use super::{
    super::{unsupported, unsupported_field},
    TextBlock,
};
use crate::{
    protocol::Capability,
    provider::ProviderError,
    schema::{
        ChatRequest, ContentPart, ImageSource, Item, Message, RawJson, Role, ToolCall, ToolResult,
    },
    wire::base64,
};

/// One conversation entry.
#[derive(Serialize)]
pub(super) struct MessageWire<'a> {
    role: &'static str,
    content: Vec<BlockWire<'a>>,
}

/// One content block of a message.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BlockWire<'a> {
    Text {
        text: &'a str,
    },
    Image {
        source: ImageSourceWire<'a>,
    },
    ToolUse {
        id: &'a str,
        name: &'a str,
        input: &'a RawJson,
    },
    ToolResult {
        tool_use_id: &'a str,
        content: ToolResultContent<'a>,
        #[serde(skip_serializing_if = "is_false")]
        is_error: bool,
    },
}

/// Where an image's bytes live.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ImageSourceWire<'a> {
    Url { url: &'a str },
    Base64 { media_type: &'a str, data: String },
}

/// A tool result's payload: the common string form, or the block list an image needs.
#[derive(Serialize)]
#[serde(untagged)]
enum ToolResultContent<'a> {
    Text(String),
    Blocks(Vec<BlockWire<'a>>),
}

/// The conversation split into the top-level system field and the message list.
pub(super) struct Conversation<'a> {
    pub(super) system: Vec<TextBlock<'a>>,
    pub(super) messages: Vec<MessageWire<'a>>,
}

/// A run of assistant items that one wire message carries.
#[derive(Default)]
struct AssistantWire<'a> {
    parts: Vec<&'a ContentPart>,
    calls: Vec<&'a ToolCall>,
}

/// Builds the conversation, folding runs and grouping tool results.
pub(super) fn build<'a>(request: &'a ChatRequest) -> Result<Conversation<'a>, ProviderError> {
    let mut system = Vec::new();
    let mut messages = Vec::new();
    let mut assistant: Option<AssistantWire<'a>> = None;
    let mut results: Vec<&'a ToolResult> = Vec::new();
    let mut spoken = false;

    for item in request.conversation() {
        match item {
            Item::Message(message) if is_system(message.role()) => {
                // The protocol carries instructions in one top-level field, so they must all precede
                // the conversation: a later system turn has no position and is refused, not hoisted.
                if spoken {
                    return Err(unsupported_field("conversation[].role"));
                }
                push_system(&mut system, message)?;
            }
            Item::Message(message) if message.role() == Role::Assistant => {
                spoken = true;
                flush_results(&mut messages, &mut results)?;
                assistant
                    .get_or_insert_with(AssistantWire::default)
                    .parts
                    .extend(message.content());
            }
            Item::ToolCall(call) => {
                spoken = true;
                flush_results(&mut messages, &mut results)?;
                assistant
                    .get_or_insert_with(AssistantWire::default)
                    .calls
                    .push(call);
            }
            // A reasoning item cannot be written back: the block needs a thinking budget this
            // request never asked for, and writing its text as visible content would change the
            // conversation.
            Item::Reasoning(_) => return Err(unsupported_field("conversation[].reasoning")),
            Item::Message(message) => {
                spoken = true;
                flush_assistant(&mut messages, &mut assistant)?;
                let mut blocks = Vec::with_capacity(results.len() + message.content().len());
                for result in results.drain(..) {
                    blocks.push(tool_result_block(result)?);
                }
                push_parts(&mut blocks, message.content())?;
                messages.push(MessageWire {
                    role: "user",
                    content: blocks,
                });
            }
            Item::ToolResult(result) => {
                spoken = true;
                flush_assistant(&mut messages, &mut assistant)?;
                results.push(result);
            }
        }
    }
    flush_assistant(&mut messages, &mut assistant)?;
    flush_results(&mut messages, &mut results)?;
    validate(&messages)?;
    Ok(Conversation { system, messages })
}

/// Reports whether a role belongs in the top-level system field.
fn is_system(role: Role) -> bool {
    matches!(role, Role::System | Role::Developer)
}

/// Appends one system turn's text blocks, refusing a part the field cannot hold.
fn push_system<'a>(
    system: &mut Vec<TextBlock<'a>>,
    message: &'a Message,
) -> Result<(), ProviderError> {
    for part in message.content() {
        match part {
            ContentPart::Text(text) => system.push(TextBlock { kind: "text", text }),
            ContentPart::Image(_) => return Err(unsupported_field("message.content")),
            ContentPart::File { .. } => return Err(unsupported(Capability::Files)),
        }
    }
    Ok(())
}

/// Writes the assistant run that ended.
fn flush_assistant<'a>(
    messages: &mut Vec<MessageWire<'a>>,
    assistant: &mut Option<AssistantWire<'a>>,
) -> Result<(), ProviderError> {
    let Some(run) = assistant.take() else {
        return Ok(());
    };
    let mut blocks = Vec::with_capacity(run.parts.len() + run.calls.len());
    for part in run.parts {
        match part {
            ContentPart::Text(text) => blocks.push(BlockWire::Text { text }),
            // The protocol carries an image in a user turn; an assistant turn has no field for one.
            ContentPart::Image(_) => return Err(unsupported_field("message.content")),
            ContentPart::File { .. } => return Err(unsupported(Capability::Files)),
        }
    }
    for call in run.calls {
        blocks.push(BlockWire::ToolUse {
            id: call.id().as_str(),
            name: call.name().as_str(),
            input: call.arguments(),
        });
    }
    // A run always carries at least one part or call; the guard keeps an empty message (which the
    // protocol refuses) unrepresentable even if that ever changes.
    if blocks.is_empty() {
        return Ok(());
    }
    messages.push(MessageWire {
        role: "assistant",
        content: blocks,
    });
    Ok(())
}

/// Writes the tool results that were waiting for a user turn.
fn flush_results<'a>(
    messages: &mut Vec<MessageWire<'a>>,
    results: &mut Vec<&'a ToolResult>,
) -> Result<(), ProviderError> {
    if results.is_empty() {
        return Ok(());
    }
    let mut blocks = Vec::with_capacity(results.len());
    for result in results.drain(..) {
        blocks.push(tool_result_block(result)?);
    }
    messages.push(MessageWire {
        role: "user",
        content: blocks,
    });
    Ok(())
}

/// Appends one message's parts as content blocks.
fn push_parts<'a>(
    blocks: &mut Vec<BlockWire<'a>>,
    parts: &'a [ContentPart],
) -> Result<(), ProviderError> {
    for part in parts {
        blocks.push(match part {
            ContentPart::Text(text) => BlockWire::Text { text },
            ContentPart::Image(source) => BlockWire::Image {
                source: image_source(source),
            },
            ContentPart::File { .. } => return Err(unsupported(Capability::Files)),
        });
    }
    Ok(())
}

/// Builds one tool-result block.
fn tool_result_block(result: &ToolResult) -> Result<BlockWire<'_>, ProviderError> {
    Ok(BlockWire::ToolResult {
        tool_use_id: result.id().as_str(),
        content: tool_result_content(result.content())?,
        is_error: result.is_error(),
    })
}

/// Builds a tool result's payload: one text part keeps the protocol's string form.
fn tool_result_content(parts: &[ContentPart]) -> Result<ToolResultContent<'_>, ProviderError> {
    if let [ContentPart::Text(text)] = parts {
        return Ok(ToolResultContent::Text(text.clone()));
    }
    let mut blocks = Vec::with_capacity(parts.len());
    push_parts(&mut blocks, parts)?;
    Ok(ToolResultContent::Blocks(blocks))
}

/// Returns the wire source of an image part.
fn image_source(source: &ImageSource) -> ImageSourceWire<'_> {
    match source {
        ImageSource::Url(url) => ImageSourceWire::Url { url },
        ImageSource::Base64 { media_type, data } => ImageSourceWire::Base64 {
            media_type,
            data: base64::encode(data),
        },
    }
}

/// Refuses a conversation that would be an empty message list or would not open with a user turn.
fn validate(messages: &[MessageWire<'_>]) -> Result<(), ProviderError> {
    if messages.is_empty() {
        // Instructions alone are not a conversation: the protocol requires at least one message.
        return Err(unsupported_field("conversation"));
    }
    if messages[0].role != "user" {
        return Err(unsupported_field("conversation[].role"));
    }
    Ok(())
}

/// Reports whether a boolean is the protocol default and may be omitted.
fn is_false(value: &bool) -> bool {
    !*value
}
