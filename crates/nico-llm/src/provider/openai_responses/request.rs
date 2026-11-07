//! Encoding a canonical request as a Responses request document.
//!
//! The document is rebuilt from the canonical model instead of copied from a client body: the
//! physical model comes from the route's map, the leading instruction turns merge into the top-level
//! `instructions` field, every other item becomes one `input` item, and every unmodelled top-level
//! field is written back from [`Extensions`].
//!
//! Unlike Chat Completions this dialect's `input` *is* an item list, so no assistant run has to be
//! folded and a tool result needs no role of its own: the canonical conversation and the wire list
//! have the same shape. The one reshape is `instructions`, a single string that has no position for a
//! system turn arriving after the conversation started.
//!
//! A field this document does not name is either preserved (`extensions`) or refused with a typed
//! error; nothing is dropped. Two kinds of refusal exist because this dialect has two kinds of absent
//! field:
//!
//! - A canonical value with no wire field here — `Sampling.stop`, `seed`, both penalties, a file part,
//!   a failed tool result, a reasoning item, an assistant-turn image. Each changes the answer, so
//!   omitting it would answer a different question; each is [`ProviderError::UnsupportedField`] or
//!   [`ProviderError::Unsupported`] and answers `422`.
//! - A *documented request member* the canonical model cannot carry and this adapter must not send
//!   behind the caller's back: `previous_response_id`, `reasoning`, `truncation`,
//!   `parallel_tool_calls`, `include`, `service_tier`, `background`, `prompt`. They can only arrive as
//!   a relayed extension key, and writing one upstream would ask for something the caller's canonical
//!   request never expressed. They are refused by name.
//!
//! `store` is written on every request as `false`. The gateway keeps no response to continue from,
//! so leaving the vendor default (`true`) in place would start retention the caller never asked the
//! gateway to enable; a relayed `store` is therefore a shadowing key and refused as a bad request.

use std::borrow::Cow;

use serde::Serialize;

use super::{PROVIDER_KIND, invalid_request, unsupported, unsupported_field};
use crate::{
    protocol::Capability,
    provider::{ProviderError, ProviderRoute},
    schema::{
        ChatRequest, ContentPart, Extensions, ImageSource, Item, Message, RawJson, ResponseFormat,
        Role, Sampling, Tool, ToolCall, ToolChoice, ToolResult,
    },
    wire::{base64, json},
};

/// The top-level fields this document writes itself.
///
/// An extension key here would be written next to a modelled field and a parser would keep only one of
/// them. The canonical request already refuses the keys *it* models; the wire spells several of the
/// same facts differently (`input` for `conversation`, `text` for `response_format`), so the adapter
/// checks its own set as well.
const MODELLED: [&str; 11] = [
    "model",
    "input",
    "instructions",
    "tools",
    "tool_choice",
    "temperature",
    "top_p",
    "max_output_tokens",
    "text",
    "stream",
    "store",
];

/// Documented request members this adapter refuses to relay.
///
/// Each one either continues a conversation the gateway does not store, asks for response data the
/// canonical answer cannot carry, or selects vendor behaviour the canonical request never expressed.
/// Writing one upstream would answer a question the caller did not ask; dropping it would answer a
/// different one. The field table in `research/responses-wire.md` records each decision.
const REFUSED: [&str; 8] = [
    "previous_response_id",
    "reasoning",
    "truncation",
    "parallel_tool_calls",
    "include",
    "service_tier",
    "background",
    "prompt",
];

/// The name a structured response uses when the canonical schema names none.
const DEFAULT_SCHEMA_NAME: &str = "response";

/// The separator two instruction turns merge with: separate turns are separate paragraphs, and
/// concatenating them without one would join two sentences into a third.
const INSTRUCTION_SEPARATOR: &str = "\n\n";

/// Encodes one canonical request.
pub(super) fn encode(
    route: &ProviderRoute,
    request: &ChatRequest,
) -> Result<Vec<u8>, ProviderError> {
    reject_relayed_fields(request)?;
    let physical = route.model(request.model());
    let input = input(request)?;
    let wire = RequestWire {
        model: physical.physical().as_str(),
        instructions: input.instructions,
        input: input.items,
        tools: tools(request.tools()),
        tool_choice: tool_choice(request.tools(), request.tool_choice()),
        sampling: SamplingWire::new(request.sampling())?,
        text: text(request.response_format()),
        stream: request.stream(),
        store: false,
        extensions: request.extensions(),
    };
    serde_json::to_vec(&wire).map_err(|error| {
        invalid_request(format!(
            "the request could not be serialized ({} error)",
            json::error_class(&error)
        ))
    })
}

/// One request document.
#[derive(Serialize)]
struct RequestWire<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    input: Vec<ItemWire<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ToolWire<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ToolChoiceWire<'a>>,
    #[serde(flatten)]
    sampling: SamplingWire,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<TextWire<'a>>,
    #[serde(skip_serializing_if = "is_false")]
    stream: bool,
    /// Always written, always `false`: the gateway stores nothing to continue from.
    store: bool,
    #[serde(flatten)]
    extensions: &'a Extensions,
}

/// One `input` item.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ItemWire<'a> {
    Message {
        role: &'static str,
        content: Vec<PartWire<'a>>,
    },
    FunctionCall {
        call_id: &'a str,
        name: &'a str,
        arguments: String,
    },
    FunctionCallOutput {
        call_id: &'a str,
        output: String,
    },
}

/// One part of a message's content.
///
/// The type tag follows the role: an input message carries `input_text`, a replayed assistant turn
/// carries the `output_text` spelling the same vendor's `output[]` uses for exactly this content.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PartWire<'a> {
    InputText { text: &'a str },
    OutputText { text: &'a str },
    InputImage { image_url: String },
}

/// One tool the model may call; this dialect declares the function flat, without a `function` object.
#[derive(Serialize)]
struct ToolWire<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
    parameters: &'a RawJson,
}

/// How the model may choose a tool: a mode, or one named function.
#[derive(Serialize)]
#[serde(untagged)]
enum ToolChoiceWire<'a> {
    Mode(&'static str),
    Named(NamedToolWire<'a>),
}

/// A reference to one declared function.
#[derive(Serialize)]
struct NamedToolWire<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    name: &'a str,
}

/// The sampling knobs this dialect carries.
#[derive(Serialize)]
struct SamplingWire {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
}

impl SamplingWire {
    /// Reads the canonical knobs, refusing the ones this dialect cannot carry.
    fn new(sampling: &Sampling) -> Result<Self, ProviderError> {
        if !sampling.stop().is_empty() {
            return Err(unsupported_field("sampling.stop"));
        }
        if sampling.seed().is_some() {
            return Err(unsupported_field("sampling.seed"));
        }
        if sampling.presence_penalty().is_some() {
            return Err(unsupported_field("sampling.presence_penalty"));
        }
        if sampling.frequency_penalty().is_some() {
            return Err(unsupported_field("sampling.frequency_penalty"));
        }
        Ok(Self {
            temperature: sampling.temperature(),
            top_p: sampling.top_p(),
            max_output_tokens: sampling.max_tokens(),
        })
    }
}

/// The structured response request.
#[derive(Serialize)]
struct TextWire<'a> {
    format: FormatWire<'a>,
}

/// How the answer's text is shaped.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum FormatWire<'a> {
    JsonObject,
    JsonSchema { name: &'a str, schema: &'a RawJson },
}

/// The conversation split into the top-level instructions and the input list.
struct Input<'a> {
    instructions: Option<String>,
    items: Vec<ItemWire<'a>>,
}

/// Builds the `input` list and the merged instructions.
fn input(request: &ChatRequest) -> Result<Input<'_>, ProviderError> {
    let mut instructions: Option<String> = None;
    let mut items = Vec::with_capacity(request.conversation().len());
    let mut spoken = false;
    for item in request.conversation() {
        match item {
            Item::Message(message) if is_system(message.role()) => {
                // The dialect carries instructions in one top-level field, so they must all precede
                // the conversation: a later instruction has no position and is refused, not hoisted.
                if spoken {
                    return Err(unsupported_field("conversation[].role"));
                }
                append_instructions(&mut instructions, message)?;
            }
            Item::Message(message) => {
                spoken = true;
                items.push(message_item(message)?);
            }
            Item::ToolCall(call) => {
                spoken = true;
                items.push(call_item(call));
            }
            Item::ToolResult(result) => {
                spoken = true;
                items.push(result_item(result)?);
            }
            // A reasoning item cannot be written back: replaying one needs both handles the canonical
            // model has no place for (`design.md` §6), and its text alone would change the turn.
            Item::Reasoning(_) => return Err(unsupported(Capability::Reasoning)),
        }
    }
    if items.is_empty() {
        // Instructions alone are not a conversation, and the dialect requires an input item.
        return Err(unsupported_field("conversation"));
    }
    Ok(Input {
        instructions,
        items,
    })
}

/// Reports whether a role belongs in the top-level instructions field.
fn is_system(role: Role) -> bool {
    matches!(role, Role::System | Role::Developer)
}

/// Merges one instruction turn into the top-level text.
fn append_instructions(
    instructions: &mut Option<String>,
    message: &Message,
) -> Result<(), ProviderError> {
    let text = joined_text(message.content(), "message.content")?;
    match instructions {
        Some(existing) => {
            existing.push_str(INSTRUCTION_SEPARATOR);
            existing.push_str(&text);
        }
        None => *instructions = Some(text),
    }
    Ok(())
}

/// Builds one message item.
fn message_item(message: &Message) -> Result<ItemWire<'_>, ProviderError> {
    let role = match message.role() {
        Role::User => "user",
        Role::Assistant => "assistant",
        // An instruction turn never reaches this function: it is folded into `instructions`.
        Role::System | Role::Developer => return Err(unsupported_field("conversation[].role")),
    };
    Ok(ItemWire::Message {
        role,
        content: parts(message.role(), message.content())?,
    })
}

/// Builds one message's content parts.
fn parts<'a>(role: Role, parts: &'a [ContentPart]) -> Result<Vec<PartWire<'a>>, ProviderError> {
    let mut wires = Vec::with_capacity(parts.len());
    for part in parts {
        wires.push(match part {
            ContentPart::Text(text) if role == Role::Assistant => PartWire::OutputText { text },
            ContentPart::Text(text) => PartWire::InputText { text },
            // An image is an input part: an assistant turn's content is output text only.
            ContentPart::Image(_) if role != Role::User => {
                return Err(unsupported_field("message.content"));
            }
            ContentPart::Image(source) => PartWire::InputImage {
                image_url: image_url(source),
            },
            ContentPart::File { .. } => return Err(unsupported(Capability::Files)),
        });
    }
    Ok(wires)
}

/// Builds one tool call; the arguments travel as the JSON string the dialect carries.
fn call_item(call: &ToolCall) -> ItemWire<'_> {
    ItemWire::FunctionCall {
        call_id: call.id().as_str(),
        name: call.name().as_str(),
        arguments: call.arguments().to_text(),
    }
}

/// Builds one tool result.
fn result_item(result: &ToolResult) -> Result<ItemWire<'_>, ProviderError> {
    // A `function_call_output` carries the output and nothing else: it has no field saying the call
    // failed, and inventing one from the text would tell the model something the tool never said.
    if result.is_error() {
        return Err(unsupported_field("tool_result.is_error"));
    }
    Ok(ItemWire::FunctionCallOutput {
        call_id: result.id().as_str(),
        output: joined_text(result.content(), "tool_result.content")?,
    })
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

/// Returns the address an image part is sent as.
fn image_url(source: &ImageSource) -> String {
    match source {
        ImageSource::Url(url) => url.clone(),
        ImageSource::Base64 { media_type, data } => {
            format!("data:{media_type};base64,{}", base64::encode(data))
        }
    }
}

/// Builds the declared tools.
fn tools(tools: &[Tool]) -> Option<Vec<ToolWire<'_>>> {
    if tools.is_empty() {
        return None;
    }
    Some(
        tools
            .iter()
            .map(|tool| ToolWire {
                kind: "function",
                name: tool.name().as_str(),
                description: tool.description(),
                parameters: tool.parameters(),
            })
            .collect(),
    )
}

/// Builds how the model may choose a tool.
///
/// A choice is only written when tools are declared: the dialect refuses a named choice on a request
/// with no `tools`, and omitting it is the same as asking for the default (`auto`).
fn tool_choice<'a>(tools: &[Tool], choice: &'a ToolChoice) -> Option<ToolChoiceWire<'a>> {
    if tools.is_empty() {
        return None;
    }
    Some(match choice {
        ToolChoice::Auto => ToolChoiceWire::Mode("auto"),
        ToolChoice::None => ToolChoiceWire::Mode("none"),
        ToolChoice::Required => ToolChoiceWire::Mode("required"),
        ToolChoice::Tool(name) => ToolChoiceWire::Named(NamedToolWire {
            kind: "function",
            name: name.as_str(),
        }),
    })
}

/// Builds the structured response request; free-form text is the protocol default and is omitted.
fn text(format: &ResponseFormat) -> Option<TextWire<'_>> {
    Some(TextWire {
        format: match format {
            ResponseFormat::Text => return None,
            ResponseFormat::JsonObject => FormatWire::JsonObject,
            ResponseFormat::JsonSchema(schema) => FormatWire::JsonSchema {
                name: schema.name().unwrap_or(DEFAULT_SCHEMA_NAME),
                schema: schema.schema(),
            },
        },
    })
}

/// Refuses a relayed field that would be written next to a modelled one, or that this adapter will
/// not send behind the caller's back.
fn reject_relayed_fields(request: &ChatRequest) -> Result<(), ProviderError> {
    for (key, _) in request.extensions().iter() {
        if MODELLED.contains(&key) {
            return Err(invalid_request(format!(
                "extension key `{key}` shadows a field of this document"
            )));
        }
        if REFUSED.contains(&key) {
            return Err(ProviderError::UnsupportedField {
                kind: PROVIDER_KIND,
                field: Cow::Owned(key.to_owned()),
            });
        }
    }
    Ok(())
}

/// Reports whether a boolean is the protocol default and may be omitted.
fn is_false(value: &bool) -> bool {
    !*value
}
