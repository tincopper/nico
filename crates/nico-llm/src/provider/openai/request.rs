//! Encoding a canonical request as a Chat Completions request document.
//!
//! The document is rebuilt from the canonical model instead of copied from a client body: the
//! physical model comes from the route's map, reasoning and tool calls are folded into the assistant
//! message that carries them, and every unmodelled top-level field is written back from
//! [`Extensions`](crate::schema::Extensions).
//!
//! A field this document does not name is either preserved (`extensions`) or refused with a typed
//! error; nothing is dropped. The conversation folding, and the refusals that belong to it, live in
//! the `message` submodule; this module owns the document around it. The refusals are the gaps a
//! later protocol matrix has to close:
//!
//! - A canonical [`ContentPart::File`](crate::schema::ContentPart::File) has no wire form here, so it
//!   is refused as an unsupported capability rather than uploaded as bytes the protocol has no field
//!   for.
//! - A reasoning item with a provider signature, a failed tool result, and an image outside a user
//!   turn have no field here either. They are refused as an unsupported *field* rather than written
//!   back as visible text, which would change the conversation the caller asked for and answer `500`
//!   for a document the gateway understood.

use serde::Serialize;

use super::invalid_request;
use crate::{
    provider::{ProviderError, ProviderRoute},
    schema::{ChatRequest, Extensions, RawJson, ResponseFormat, Sampling, Tool, ToolChoice},
};

mod message;

use message::MessageWire;

/// The top-level fields this document writes itself.
///
/// An extension key here would be written next to a modelled field and a parser would keep only one of
/// them. The canonical request already refuses the keys *it* models; the wire spells several of the
/// same facts differently (`messages` for `conversation`, `temperature` for `sampling.temperature`),
/// so the adapter checks its own set as well.
const MODELLED: [&str; 13] = [
    "model",
    "messages",
    "tools",
    "tool_choice",
    "temperature",
    "top_p",
    "max_tokens",
    "stop",
    "seed",
    "presence_penalty",
    "frequency_penalty",
    "response_format",
    "stream",
];

/// The document's default for a structured response that names no schema.
const DEFAULT_SCHEMA_NAME: &str = "response";

/// Encodes one canonical request.
pub(super) fn encode(
    route: &ProviderRoute,
    request: &ChatRequest,
) -> Result<Vec<u8>, ProviderError> {
    reject_shadowing_extensions(request)?;
    let physical = route.model(request.model());
    let wire = RequestWire {
        model: physical.physical().as_str(),
        messages: message::messages(request)?,
        tools: tools(request.tools()),
        tool_choice: tool_choice(request.tools(), request.tool_choice()),
        sampling: SamplingWire::new(request.sampling()),
        response_format: response_format(request.response_format()),
        stream: request.stream(),
        extensions: request.extensions(),
    };
    serde_json::to_vec(&wire).map_err(|error| {
        invalid_request(format!(
            "the request could not be serialized ({} error)",
            super::error_class(&error)
        ))
    })
}

/// Refuses an extension key that would be written next to a modelled field.
fn reject_shadowing_extensions(request: &ChatRequest) -> Result<(), ProviderError> {
    for (key, _) in request.extensions().iter() {
        if MODELLED.contains(&key) {
            return Err(invalid_request(format!(
                "extension key `{key}` shadows a field of this document"
            )));
        }
    }
    Ok(())
}

/// One request document.
#[derive(Serialize)]
struct RequestWire<'a> {
    model: &'a str,
    messages: Vec<MessageWire<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ToolWire<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ToolChoiceWire<'a>>,
    #[serde(flatten)]
    sampling: SamplingWire<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormatWire<'a>>,
    #[serde(skip_serializing_if = "is_false")]
    stream: bool,
    #[serde(flatten)]
    extensions: &'a Extensions,
}

/// One tool the model may call.
#[derive(Serialize)]
struct ToolWire<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: FunctionDeclWire<'a>,
}

/// A tool's name, description and parameter schema.
#[derive(Serialize)]
struct FunctionDeclWire<'a> {
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
    Named(NamedToolChoiceWire<'a>),
}

/// A reference to one declared function.
#[derive(Serialize)]
struct NamedToolChoiceWire<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: NamedFunctionWire<'a>,
}

/// The name inside a function reference.
#[derive(Serialize)]
struct NamedFunctionWire<'a> {
    name: &'a str,
}

/// The sampling knobs this protocol carries.
#[derive(Serialize)]
struct SamplingWire<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "is_empty")]
    stop: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    presence_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frequency_penalty: Option<f64>,
}

impl<'a> SamplingWire<'a> {
    /// Reads the canonical knobs.
    ///
    /// The output ceiling is written as `max_tokens`, the spelling every OpenAI-compatible endpoint
    /// accepts. The newer `max_completion_tokens` is what OpenAI's current models recommend, and a
    /// route that needs it will have to say so at the route level rather than have this adapter guess
    /// from a model name.
    fn new(sampling: &'a Sampling) -> Self {
        Self {
            temperature: sampling.temperature(),
            top_p: sampling.top_p(),
            max_tokens: sampling.max_tokens(),
            stop: sampling.stop(),
            seed: sampling.seed(),
            presence_penalty: sampling.presence_penalty(),
            frequency_penalty: sampling.frequency_penalty(),
        }
    }
}

/// The response-format request.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponseFormatWire<'a> {
    JsonObject,
    JsonSchema { json_schema: SchemaWire<'a> },
}

/// The JSON-Schema contract of a structured response.
///
/// `strict` is not written: the canonical model has no such field, and emitting `true` would promise
/// a decoding guarantee the caller never asked for.
#[derive(Serialize)]
struct SchemaWire<'a> {
    name: &'a str,
    schema: &'a RawJson,
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
                function: FunctionDeclWire {
                    name: tool.name().as_str(),
                    description: tool.description(),
                    parameters: tool.parameters(),
                },
            })
            .collect(),
    )
}

/// Builds how the model may choose a tool.
///
/// A choice is only written when tools are declared: the protocol refuses `tool_choice` on a request
/// with no `tools`, and omitting it is the same as asking for the default (`auto`).
fn tool_choice<'a>(tools: &[Tool], choice: &'a ToolChoice) -> Option<ToolChoiceWire<'a>> {
    if tools.is_empty() {
        return None;
    }
    Some(match choice {
        ToolChoice::Auto => ToolChoiceWire::Mode("auto"),
        ToolChoice::None => ToolChoiceWire::Mode("none"),
        ToolChoice::Required => ToolChoiceWire::Mode("required"),
        ToolChoice::Tool(name) => ToolChoiceWire::Named(NamedToolChoiceWire {
            kind: "function",
            function: NamedFunctionWire {
                name: name.as_str(),
            },
        }),
    })
}

/// Builds the response-format request; free-form text is the protocol default and is omitted.
fn response_format(format: &ResponseFormat) -> Option<ResponseFormatWire<'_>> {
    match format {
        ResponseFormat::Text => None,
        ResponseFormat::JsonObject => Some(ResponseFormatWire::JsonObject),
        ResponseFormat::JsonSchema(schema) => Some(ResponseFormatWire::JsonSchema {
            json_schema: SchemaWire {
                name: schema.name().unwrap_or(DEFAULT_SCHEMA_NAME),
                schema: schema.schema(),
            },
        }),
    }
}

/// Reports whether a boolean is the protocol default and may be omitted.
fn is_false(value: &bool) -> bool {
    !*value
}

/// Reports whether a slice is empty and may be omitted.
fn is_empty<T>(value: &[T]) -> bool {
    value.is_empty()
}
