//! Encoding a canonical request as a Messages request document.
//!
//! The document is rebuilt from the canonical model instead of copied from a client body: the
//! physical model comes from the route's map, the leading system turns become the top-level `system`
//! field, a run of assistant items becomes one message, and every unmodelled top-level field is
//! written back from [`Extensions`]. The conversation folding lives in [`conversation`], because it is
//! a state machine over items while this module owns the document around it.
//!
//! A field this document does not name is either preserved (`extensions`) or refused with a typed
//! error; nothing is dropped. The refusals are the gaps the protocol matrix records:
//!
//! - `seed` and the two penalties have no field, and `temperature` above 1.0 is outside this
//!   protocol's range. They change the answer, so they are refused rather than omitted or clamped.
//! - The protocol has no JSON mode, so a structured response format is an undeclared capability.
//! - A reasoning item cannot be sent back: writing a `thinking` block requires enabling thinking,
//!   which the canonical model cannot ask for.
//!
//! `max_tokens` is always written: the caller's ceiling wins, and the route's `max_output_tokens`
//! answers for a request that names none.

use serde::Serialize;

use super::{unsupported, unsupported_field};
use crate::{
    protocol::Capability,
    provider::{ProviderError, ProviderRoute},
    schema::{ChatRequest, Extensions, RawJson, ResponseFormat, Sampling, Tool, ToolChoice},
    wire::json,
};

mod conversation;

use conversation::MessageWire;

/// The top-level fields this document writes itself.
///
/// An extension key here would be written next to a modelled field and a parser would keep only one of
/// them. The canonical request already refuses the keys *it* models; the wire spells several of the
/// same facts differently (`messages` for `conversation`, `stop_sequences` for `sampling.stop`), so
/// the adapter checks its own set as well.
const MODELLED: [&str; 10] = [
    "model",
    "max_tokens",
    "system",
    "messages",
    "tools",
    "tool_choice",
    "temperature",
    "top_p",
    "stop_sequences",
    "stream",
];

/// The highest `temperature` this protocol accepts.
///
/// The canonical model takes the widest range any target allows (2.0); this protocol's own range is
/// narrower, and a value outside it is refused only here, where the document is written.
const MAX_TEMPERATURE: f64 = 1.0;

/// Encodes one canonical request.
pub(super) fn encode(
    route: &ProviderRoute,
    request: &ChatRequest,
) -> Result<Vec<u8>, ProviderError> {
    reject_shadowing_extensions(request)?;
    reject_response_format(request)?;
    let physical = route.model(request.model());
    let conversation = conversation::build(request)?;
    let wire = RequestWire {
        model: physical.physical().as_str(),
        max_tokens: ceiling(route, request.sampling())?,
        system: conversation.system,
        messages: conversation.messages,
        tools: tools(request.tools()),
        tool_choice: tool_choice(request.tools(), request.tool_choice()),
        sampling: SamplingWire::new(request.sampling())?,
        stream: request.stream(),
        extensions: request.extensions(),
    };
    serde_json::to_vec(&wire).map_err(|error| {
        super::invalid_request(format!(
            "the request could not be serialized ({} error)",
            json::error_class(&error)
        ))
    })
}

/// One request document.
#[derive(Serialize)]
struct RequestWire<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "is_empty")]
    system: Vec<TextBlock<'a>>,
    messages: Vec<MessageWire<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ToolWire<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ToolChoiceWire<'a>>,
    #[serde(flatten)]
    sampling: SamplingWire<'a>,
    #[serde(skip_serializing_if = "is_false")]
    stream: bool,
    #[serde(flatten)]
    extensions: &'a Extensions,
}

/// One top-level system text block.
///
/// The field is always the block list, never the single string the protocol also accepts: one
/// spelling keeps a golden fixture from having two forms of the same request.
#[derive(Serialize)]
pub(super) struct TextBlock<'a> {
    #[serde(rename = "type")]
    pub(super) kind: &'static str,
    pub(super) text: &'a str,
}

/// One tool the model may call.
#[derive(Serialize)]
struct ToolWire<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
    input_schema: &'a RawJson,
}

/// How the model may choose a tool.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ToolChoiceWire<'a> {
    Auto,
    Any,
    None,
    Tool { name: &'a str },
}

/// The sampling knobs this protocol carries.
#[derive(Serialize)]
struct SamplingWire<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "is_empty")]
    stop_sequences: &'a [String],
}

impl<'a> SamplingWire<'a> {
    /// Reads the canonical knobs, refusing the ones this protocol cannot carry.
    fn new(sampling: &'a Sampling) -> Result<Self, ProviderError> {
        if let Some(temperature) = sampling.temperature()
            && temperature > MAX_TEMPERATURE
        {
            return Err(unsupported_field("temperature"));
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
            stop_sequences: sampling.stop(),
        })
    }
}

/// Returns the output ceiling this request is written with.
///
/// The caller's value always wins. Without one, the route's ceiling answers; a route that has none
/// would have been refused when its endpoint was compiled, so reaching that case means an encoder ran
/// without one — a publication defect, not a caller error.
fn ceiling(route: &ProviderRoute, sampling: &Sampling) -> Result<u32, ProviderError> {
    if let Some(max_tokens) = sampling.max_tokens() {
        return Ok(max_tokens);
    }
    super::url::output_ceiling(route)
        .map(std::num::NonZeroU32::get)
        .map_err(|source| ProviderError::InvalidEndpoint {
            kind: super::PROVIDER_KIND,
            source,
        })
}

/// Refuses a structured response format: this protocol has no JSON mode.
fn reject_response_format(request: &ChatRequest) -> Result<(), ProviderError> {
    match request.response_format() {
        ResponseFormat::Text => Ok(()),
        ResponseFormat::JsonObject | ResponseFormat::JsonSchema(_) => {
            Err(unsupported(Capability::ResponseFormat))
        }
    }
}

/// Refuses an extension key that would be written next to a modelled field.
fn reject_shadowing_extensions(request: &ChatRequest) -> Result<(), ProviderError> {
    for (key, _) in request.extensions().iter() {
        if MODELLED.contains(&key) {
            return Err(super::invalid_request(format!(
                "extension key `{key}` shadows a field of this document"
            )));
        }
    }
    Ok(())
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
                name: tool.name().as_str(),
                description: tool.description(),
                input_schema: tool.parameters(),
            })
            .collect(),
    )
}

/// Builds how the model may choose a tool.
///
/// A choice is only written when tools are declared: the protocol refuses `any` or a named tool on a
/// request with no `tools`, and omitting it is the same as asking for the default (`auto`).
fn tool_choice<'a>(tools: &[Tool], choice: &'a ToolChoice) -> Option<ToolChoiceWire<'a>> {
    if tools.is_empty() {
        return None;
    }
    Some(match choice {
        ToolChoice::Auto => ToolChoiceWire::Auto,
        ToolChoice::None => ToolChoiceWire::None,
        ToolChoice::Required => ToolChoiceWire::Any,
        ToolChoice::Tool(name) => ToolChoiceWire::Tool {
            name: name.as_str(),
        },
    })
}

/// Reports whether a boolean is the protocol default and may be omitted.
fn is_false(value: &bool) -> bool {
    !*value
}

/// Reports whether a slice is empty and may be omitted.
fn is_empty<T>(value: &[T]) -> bool {
    value.is_empty()
}
