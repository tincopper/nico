//! Encoding a canonical request as a `generateContent` request document.
//!
//! The document is rebuilt from the canonical model instead of copied from a client body. The model
//! is not written here at all: this dialect names it in the request path, which the adapter pins from
//! the route facts ([`super::url`]). Every other canonical fact has a member below, and every
//! unmodelled top-level field is written back from [`Extensions`].
//!
//! The conversation itself — turns, runs, tool calls and results — is built by [`conversation`],
//! because it is a state machine over items while this module owns the document around it.
//!
//! # Refusals
//!
//! A canonical shape with no member here is refused, never dropped. The conversation's own gaps (a
//! remote image, a file part, a tool result that is not plain text, a failed result, an instruction
//! that carries an image) live in [`conversation`]; this module's are:
//!
//! - an extension key that would be written next to a modelled member, or that would name a model
//!   beside the one the path pinned;
//! - a document that cannot be serialized at all, which is a defect of this crate and not of the
//!   caller's request.
//!
//! `Sampling` needs no refusal: this dialect carries every knob the canonical model names, including
//! `seed` and the two penalties, so `generationConfig` is complete rather than lossy.
//!
//! The ceiling is written only when the request names one. `providers[].max_output_tokens` is not
//! required by this dialect and is not applied as a default — the same rule the OpenAI dialects
//! follow — so a request that names none omits `maxOutputTokens` rather than inventing a limit.

use serde::Serialize;

use super::{invalid_request, parts::PartWire};
use crate::{
    provider::{ProviderError, ProviderRoute},
    schema::{ChatRequest, Extensions, RawJson, ResponseFormat, Sampling, Tool, ToolChoice},
    wire::json,
};

mod conversation;

use conversation::Conversation;

/// The MIME type a structured response is asked for with.
const JSON_MIME: &str = "application/json";

/// The top-level fields this document writes itself, plus the one it deliberately never writes.
///
/// An extension key here would be written next to a modelled member and a parser would keep only one
/// of them. The canonical request already refuses the keys *it* models; this document spells several
/// of the same facts differently (`contents` for `conversation`, `generationConfig` for `sampling`),
/// so the adapter checks its own set as well.
///
/// `model` is in the set although this document does not write it: this dialect also accepts the
/// model as a body member, and an extension that carried one would name a model beside the path's
/// pinned one. Two disagreeing models in one request are exactly the silent switch the pinned path
/// exists to prevent.
const MODELLED: [&str; 6] = [
    "model",
    "contents",
    "systemInstruction",
    "tools",
    "toolConfig",
    "generationConfig",
];

/// Encodes one canonical request.
///
/// The route is not read: this dialect names the physical model in the request path, and the exchange
/// refused a request that resolved to a different model before calling this encoder.
///
/// # Errors
/// Returns [`ProviderError::UnsupportedField`] or [`ProviderError::Unsupported`] for a canonical value
/// this document cannot carry, and [`ProviderError::InvalidRequest`] for an extension key that would
/// shadow a member of it or a document that cannot be serialized.
pub(super) fn encode(
    _route: &ProviderRoute,
    request: &ChatRequest,
) -> Result<Vec<u8>, ProviderError> {
    reject_shadowing_extensions(request)?;
    let conversation = Conversation::build(request)?;
    let wire = RequestWire {
        contents: conversation.contents,
        system_instruction: conversation.system,
        tools: tools(request.tools()),
        tool_config: tool_config(request.tools(), request.tool_choice()),
        generation_config: GenerationConfigWire::build(
            request.sampling(),
            request.response_format(),
        ),
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
    contents: Vec<ContentWire<'a>>,
    #[serde(rename = "systemInstruction", skip_serializing_if = "Option::is_none")]
    system_instruction: Option<ContentWire<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ToolsWire<'a>>>,
    #[serde(rename = "toolConfig", skip_serializing_if = "Option::is_none")]
    tool_config: Option<ToolConfigWire<'a>>,
    #[serde(rename = "generationConfig", skip_serializing_if = "Option::is_none")]
    generation_config: Option<GenerationConfigWire<'a>>,
    #[serde(flatten)]
    extensions: &'a Extensions,
}

/// One turn, or the system instruction (which carries no role).
#[derive(Serialize)]
pub(super) struct ContentWire<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) role: Option<&'static str>,
    pub(super) parts: Vec<PartWire<'a>>,
}

/// The tool list: this dialect groups declarations under one object.
#[derive(Serialize)]
struct ToolsWire<'a> {
    #[serde(rename = "functionDeclarations")]
    declarations: Vec<DeclarationWire<'a>>,
}

/// One function the model may call.
#[derive(Serialize)]
struct DeclarationWire<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
    parameters: &'a RawJson,
}

/// How the model may choose a tool.
#[derive(Serialize)]
struct ToolConfigWire<'a> {
    #[serde(rename = "functionCallingConfig")]
    config: FunctionCallingConfigWire<'a>,
}

/// The mode and, for a named tool, the names allowed.
#[derive(Serialize)]
struct FunctionCallingConfigWire<'a> {
    mode: &'static str,
    #[serde(
        rename = "allowedFunctionNames",
        skip_serializing_if = "Option::is_none"
    )]
    allowed: Option<Vec<&'a str>>,
}

/// The sampling and output-shape knobs of one request.
#[derive(Serialize, Default)]
struct GenerationConfigWire<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(rename = "topP", skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(rename = "maxOutputTokens", skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(rename = "stopSequences", skip_serializing_if = "is_empty")]
    stop_sequences: &'a [String],
    #[serde(rename = "presencePenalty", skip_serializing_if = "Option::is_none")]
    presence_penalty: Option<f64>,
    #[serde(rename = "frequencyPenalty", skip_serializing_if = "Option::is_none")]
    frequency_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<u64>,
    #[serde(rename = "responseMimeType", skip_serializing_if = "Option::is_none")]
    response_mime_type: Option<&'static str>,
    #[serde(rename = "responseSchema", skip_serializing_if = "Option::is_none")]
    response_schema: Option<&'a RawJson>,
}

impl<'a> GenerationConfigWire<'a> {
    /// Reads the canonical knobs and the requested output shape.
    ///
    /// Nothing is refused: every canonical knob has a member here. A document with no knob at all is
    /// not written, because an empty object would tell the peer the caller configured
    /// `generationConfig` and then configured nothing.
    fn build(sampling: &'a Sampling, format: &'a ResponseFormat) -> Option<Self> {
        let mut config = Self {
            temperature: sampling.temperature(),
            top_p: sampling.top_p(),
            max_output_tokens: sampling.max_tokens(),
            stop_sequences: sampling.stop(),
            presence_penalty: sampling.presence_penalty(),
            frequency_penalty: sampling.frequency_penalty(),
            seed: sampling.seed(),
            ..Self::default()
        };
        match format {
            ResponseFormat::Text => {}
            ResponseFormat::JsonObject => config.response_mime_type = Some(JSON_MIME),
            ResponseFormat::JsonSchema(schema) => {
                config.response_mime_type = Some(JSON_MIME);
                // The schema's name is a label a caller uses to identify it; this dialect identifies
                // a schema by its position, and a name invented here would be a member the peer
                // rejects. The constraint itself is written verbatim.
                config.response_schema = Some(schema.schema());
            }
        }
        (!config.is_empty()).then_some(config)
    }

    /// Reports whether every knob is unset.
    fn is_empty(&self) -> bool {
        self.temperature.is_none()
            && self.top_p.is_none()
            && self.max_output_tokens.is_none()
            && self.stop_sequences.is_empty()
            && self.presence_penalty.is_none()
            && self.frequency_penalty.is_none()
            && self.seed.is_none()
            && self.response_mime_type.is_none()
            && self.response_schema.is_none()
    }
}

/// Builds the declared tools.
fn tools(tools: &[Tool]) -> Option<Vec<ToolsWire<'_>>> {
    if tools.is_empty() {
        return None;
    }
    Some(vec![ToolsWire {
        declarations: tools
            .iter()
            .map(|tool| DeclarationWire {
                name: tool.name().as_str(),
                description: tool.description(),
                parameters: tool.parameters(),
            })
            .collect(),
    }])
}

/// Builds how the model may choose a tool.
///
/// A choice is only written when tools are declared: `ANY` on a request with no declarations asks
/// for a call the model cannot make.
fn tool_config<'a>(tools: &[Tool], choice: &'a ToolChoice) -> Option<ToolConfigWire<'a>> {
    if tools.is_empty() {
        return None;
    }
    let config = match choice {
        ToolChoice::Auto => FunctionCallingConfigWire {
            mode: "AUTO",
            allowed: None,
        },
        ToolChoice::None => FunctionCallingConfigWire {
            mode: "NONE",
            allowed: None,
        },
        ToolChoice::Required => FunctionCallingConfigWire {
            mode: "ANY",
            allowed: None,
        },
        ToolChoice::Tool(name) => FunctionCallingConfigWire {
            mode: "ANY",
            allowed: Some(vec![name.as_str()]),
        },
    };
    Some(ToolConfigWire { config })
}

/// Refuses an extension key that would be written next to a modelled member.
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

/// Reports whether a slice is empty and may be omitted.
fn is_empty<T>(value: &[T]) -> bool {
    value.is_empty()
}
