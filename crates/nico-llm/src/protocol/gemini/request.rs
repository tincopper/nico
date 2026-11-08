//! Decoding a `generateContent` request: the target's model, then the body.
//!
//! The document splits the conversation across four members: `systemInstruction` is the system
//! prompt, `contents[]` holds the turns as `parts[]` lists, `tools`/`toolConfig` declare the
//! functions, and `generationConfig` carries the sampling knobs. A part is this protocol's only
//! content shape, and text, an image, a thought, a tool call and a tool result all live in the same
//! list, so the conversation is folded part by part (see [`parts`]): text accumulates into a
//! message, and a call, a result or a thought becomes a canonical item of its own.
//!
//! # What is refused
//!
//! Nothing is dropped silently. The members this dialect defines and the canonical model cannot
//! carry are refused by name: a `candidateCount` above one, `safetySettings`, `cachedContent`,
//! `thinkingConfig` (the canonical model has no reasoning budget), `topK` (the canonical `Sampling`
//! is frozen; §10), and the built-in tools. A `model` member in the body is a bad document rather
//! than a preserved field — the request target already names the model, and two disagreeing models
//! in one request are the silent switch the pinned target exists to prevent. Everything else the
//! body carries at the top level is preserved in [`ChatRequest::extensions`].

use std::collections::BTreeMap;

use serde::Deserialize;

use super::{field, invalid, parts, reject_extra, schema, target, unsupported};
use crate::{
    protocol::{CodecError, RequestFacts},
    schema::{
        ChatRequest, ContentPart, Extensions, Item, Message, RawJson, ResponseFormat, Role,
        Sampling, Tool, ToolChoice, ToolName,
    },
};

/// The roles `contents[]` may name; the system prompt is `systemInstruction`, not a turn.
mod role {
    pub(super) const USER: &str = "user";
    pub(super) const MODEL: &str = "model";
}

/// Decodes one buffered request body and its target facts.
pub(super) fn decode(facts: &RequestFacts, body: &[u8]) -> Result<ChatRequest, CodecError> {
    let target = target::parse(facts.path())?;
    let wire: RequestWire = serde_json::from_slice(body)
        .map_err(|error| invalid(format!("the request body is not a valid document: {error}")))?;
    reject_body_model(&wire)?;
    let conversation = conversation(&wire)?;
    let sampling = sampling(&wire)?;
    let tools = tools(&wire)?;
    let tool_choice = tool_choice(wire.tool_config.as_ref())?;
    let response_format = response_format(wire.generation_config.as_ref())?;

    let mut request = ChatRequest::try_new(target.model, conversation)
        .map_err(schema)?
        .with_sampling(sampling)
        .with_response_format(response_format)
        .with_extensions(extensions(&wire)?);
    if !tools.is_empty() {
        request = request.with_tools(tools).map_err(schema)?;
    }
    request = request.with_tool_choice(tool_choice).map_err(schema)?;
    Ok(request.with_stream(target.stream))
}

/// Refuses a body that carries a model beside the one the path pinned.
fn reject_body_model(wire: &RequestWire) -> Result<(), CodecError> {
    if wire.model.is_some() {
        return Err(invalid(
            "field `model` is not part of this document: the request target names the model",
        ));
    }
    Ok(())
}

/// Builds the canonical conversation from the system instruction and the turns.
fn conversation(wire: &RequestWire) -> Result<Vec<Item>, CodecError> {
    let mut items = Vec::new();
    if let Some(system) = &wire.system_instruction {
        items.push(Item::Message(system_message(system)?));
    }
    if wire.contents.is_empty() {
        return Err(invalid("field `contents` carries no turn"));
    }
    let mut calls = Vec::new();
    for (index, content) in wire.contents.iter().enumerate() {
        reject_extra(&content.extra, &format!("contents[{index}]"))?;
        parts::turn(content, index, &mut calls, &mut items)?;
    }
    Ok(items)
}

/// Builds the system prompt: text parts only, as one leading message.
fn system_message(wire: &ContentWire) -> Result<Message, CodecError> {
    reject_extra(&wire.extra, "systemInstruction")?;
    let mut content = Vec::with_capacity(wire.parts.len());
    for (index, part) in wire.parts.iter().enumerate() {
        match parts::decode_part(part, &format!("systemInstruction.parts[{index}]"))? {
            parts::Piece::Text(text) => content.push(ContentPart::text(text)),
            // The instruction is a parts list like any turn, but the canonical system message is
            // text-only: an image, a call or a thought there has no position the caller asked for.
            _ => return Err(unsupported("systemInstruction.parts")),
        }
    }
    Message::try_new(Role::System, content).map_err(|error| field("systemInstruction", error))
}

/// Refuses the knobs `generationConfig` defines and the canonical model cannot carry.
fn check_generation_config(config: &GenerationConfigWire) -> Result<(), CodecError> {
    if config.top_k.is_some() {
        return Err(unsupported("generationConfig.topK"));
    }
    if config.thinking_config.is_some() {
        return Err(unsupported("generationConfig.thinkingConfig"));
    }
    if config.candidate_count.is_some_and(|count| count > 1) {
        return Err(unsupported("generationConfig.candidateCount"));
    }
    reject_extra(&config.extra, "generationConfig")
}

/// Builds the sampling knobs: this dialect carries every canonical one, so nothing is refused here.
fn sampling(wire: &RequestWire) -> Result<Sampling, CodecError> {
    let Some(config) = &wire.generation_config else {
        return Ok(Sampling::default());
    };
    check_generation_config(config)?;
    let max_tokens = match config.max_output_tokens {
        Some(value) => Some(u32::try_from(value).map_err(|_| {
            invalid("field `generationConfig.maxOutputTokens` does not fit this gateway")
        })?),
        None => None,
    };
    let sampling = Sampling::try_new(config.temperature, config.top_p, max_tokens)
        .map_err(|error| field("generationConfig", error))?;
    let sampling = sampling
        .with_stop(config.stop_sequences.clone())
        .map_err(|error| field("generationConfig.stopSequences", error))?;
    let sampling = match config.presence_penalty {
        Some(penalty) => sampling
            .with_presence_penalty(penalty)
            .map_err(|error| field("generationConfig.presencePenalty", error))?,
        None => sampling,
    };
    let sampling = match config.frequency_penalty {
        Some(penalty) => sampling
            .with_frequency_penalty(penalty)
            .map_err(|error| field("generationConfig.frequencyPenalty", error))?,
        None => sampling,
    };
    Ok(match config.seed {
        Some(seed) => sampling.with_seed(seed),
        None => sampling,
    })
}

/// Builds the declared functions, refusing the tools that run at the provider.
fn tools(wire: &RequestWire) -> Result<Vec<Tool>, CodecError> {
    let mut tools = Vec::new();
    for (index, group) in wire.tools.iter().enumerate() {
        if let Some(key) = group.extra.keys().next() {
            return Err(unsupported(format!("tools[{index}].{key}")));
        }
        for (position, declaration) in group.declarations.iter().enumerate() {
            let path = format!("tools[{index}].functionDeclarations[{position}]");
            if let Some(key) = declaration.extra.keys().next() {
                return Err(unsupported(super::named(&path, key)));
            }
            let name =
                ToolName::try_new(declaration.name.clone()).map_err(|error| field(&path, error))?;
            let mut tool = Tool::try_new(name, declaration.parameters.clone())
                .map_err(|error| field(&path, error))?;
            if let Some(description) = &declaration.description {
                tool = tool
                    .with_description(description.clone())
                    .map_err(|error| field(&format!("{path}.description"), error))?;
            }
            tools.push(tool);
        }
    }
    Ok(tools)
}

/// Builds how the model may choose a tool.
///
/// A named choice is validated by the canonical request against the declared tools, so this function
/// only maps the wire's modes; `allowedFunctionNames` beside any mode but `ANY` is a misshapen
/// document, and more than one name beside `ANY` has no canonical position (one choice, one tool).
fn tool_choice(wire: Option<&ToolConfigWire>) -> Result<ToolChoice, CodecError> {
    let Some(config) = wire else {
        return Ok(ToolChoice::Auto);
    };
    reject_extra(&config.extra, "toolConfig")?;
    let Some(config) = &config.config else {
        return Ok(ToolChoice::Auto);
    };
    reject_extra(&config.extra, "toolConfig.functionCallingConfig")?;
    let allowed = config.allowed.as_deref().unwrap_or_default();
    match config.mode.as_deref() {
        Some("ANY") if allowed.len() <= 1 => match allowed.first() {
            Some(name) => Ok(ToolChoice::Tool(ToolName::try_new(name.clone()).map_err(
                |error| {
                    field(
                        "toolConfig.functionCallingConfig.allowedFunctionNames",
                        error,
                    )
                },
            )?)),
            None => Ok(ToolChoice::Required),
        },
        Some("ANY") => Err(unsupported(
            "toolConfig.functionCallingConfig.allowedFunctionNames",
        )),
        Some("AUTO" | "NONE") if allowed.is_empty() => {
            if config.mode.as_deref() == Some("NONE") {
                Ok(ToolChoice::None)
            } else {
                Ok(ToolChoice::Auto)
            }
        }
        Some("AUTO" | "NONE") => Err(invalid(
            "field `toolConfig.functionCallingConfig.allowedFunctionNames` requires mode `ANY`",
        )),
        Some(mode) => Err(invalid(format!(
            "field `toolConfig.functionCallingConfig.mode` is not a value this protocol defines: \
             `{mode}`"
        ))),
        None => Err(invalid(
            "field `toolConfig.functionCallingConfig.mode` is required when the config is present",
        )),
    }
}

/// Builds the structured-output request.
fn response_format(wire: Option<&GenerationConfigWire>) -> Result<ResponseFormat, CodecError> {
    let Some(config) = wire else {
        return Ok(ResponseFormat::Text);
    };
    let Some(mime) = config.response_mime_type.as_deref() else {
        return if config.response_schema.is_some() {
            Err(invalid(
                "field `generationConfig.responseSchema` requires \
                 `generationConfig.responseMimeType` to be `application/json`",
            ))
        } else {
            Ok(ResponseFormat::Text)
        };
    };
    match mime {
        "application/json" => match &config.response_schema {
            Some(document) => Ok(ResponseFormat::JsonSchema(
                crate::schema::JsonSchema::try_new(None, document.clone()).map_err(schema)?,
            )),
            None => Ok(ResponseFormat::JsonObject),
        },
        // The protocol's default spelling carries nothing the absence of the member would not.
        "text/plain" => Ok(ResponseFormat::Text),
        _ => Err(unsupported("generationConfig.responseMimeType")),
    }
}

/// Collects the fields this codec preserves, refusing the ones it knows it cannot carry.
fn extensions(wire: &RequestWire) -> Result<Extensions, CodecError> {
    let mut extensions = Extensions::new();
    if let Some(labels) = &wire.labels {
        extensions
            .try_insert("labels", labels.clone())
            .map_err(schema)?;
    }
    for (key, value) in &wire.extra {
        if REFUSED.contains(&key.as_str()) {
            return Err(unsupported(key.clone()));
        }
        extensions
            .try_insert(key.clone(), value.clone())
            .map_err(schema)?;
    }
    Ok(extensions)
}

/// Top-level keys this codec knows and cannot carry.
const REFUSED: [&str; 2] = ["safetySettings", "cachedContent"];

/// One request body.
#[derive(Deserialize)]
struct RequestWire {
    /// Not part of a body in this protocol; present means a bad document (see the module doc).
    #[serde(default)]
    model: Option<RawJson>,
    #[serde(default)]
    contents: Vec<ContentWire>,
    #[serde(default, rename = "systemInstruction")]
    system_instruction: Option<ContentWire>,
    #[serde(default)]
    tools: Vec<ToolsWire>,
    #[serde(default, rename = "toolConfig")]
    tool_config: Option<ToolConfigWire>,
    #[serde(default, rename = "generationConfig")]
    generation_config: Option<GenerationConfigWire>,
    #[serde(default)]
    labels: Option<RawJson>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

/// One turn, or the system instruction (which carries no role).
#[derive(Deserialize)]
pub(super) struct ContentWire {
    #[serde(default)]
    pub(super) role: Option<String>,
    #[serde(default)]
    pub(super) parts: Vec<RawJson>,
    #[serde(flatten)]
    pub(super) extra: BTreeMap<String, RawJson>,
}

impl ContentWire {
    /// Returns the role a turn names, defaulting to the caller's.
    ///
    /// The protocol documents the role as optional for single-turn requests, and a request without
    /// roles asks the model to answer the caller, which is the user's position.
    pub(super) fn role(&self, index: usize) -> Result<Role, CodecError> {
        match self.role.as_deref() {
            None | Some(role::USER) => Ok(Role::User),
            Some(role::MODEL) => Ok(Role::Assistant),
            Some(other) => Err(invalid(format!(
                "field `contents[{index}].role` is not a value this protocol defines: `{other}`"
            ))),
        }
    }
}

/// The tool list: this dialect groups declarations under one object.
#[derive(Deserialize)]
struct ToolsWire {
    #[serde(default, rename = "functionDeclarations")]
    declarations: Vec<DeclarationWire>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

/// One function the model may call.
#[derive(Deserialize)]
struct DeclarationWire {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default = "empty_object")]
    parameters: RawJson,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

/// How the model may choose a tool.
#[derive(Deserialize)]
struct ToolConfigWire {
    #[serde(default, rename = "functionCallingConfig")]
    config: Option<FunctionCallingConfigWire>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

/// The mode and, for a named tool, the names allowed.
#[derive(Deserialize)]
struct FunctionCallingConfigWire {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default, rename = "allowedFunctionNames")]
    allowed: Option<Vec<String>>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

/// The sampling and output-shape knobs of one request.
#[derive(Deserialize)]
struct GenerationConfigWire {
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default, rename = "topP")]
    top_p: Option<f64>,
    #[serde(default, rename = "maxOutputTokens")]
    max_output_tokens: Option<u64>,
    #[serde(default, rename = "stopSequences")]
    stop_sequences: Vec<String>,
    #[serde(default, rename = "presencePenalty")]
    presence_penalty: Option<f64>,
    #[serde(default, rename = "frequencyPenalty")]
    frequency_penalty: Option<f64>,
    #[serde(default)]
    seed: Option<u64>,
    #[serde(default, rename = "responseMimeType")]
    response_mime_type: Option<String>,
    #[serde(default, rename = "responseSchema")]
    response_schema: Option<RawJson>,
    /// Refused: the canonical `Sampling` is frozen (§10).
    #[serde(default, rename = "topK")]
    top_k: Option<RawJson>,
    /// Refused above one; one candidate is what the canonical model produces anyway.
    #[serde(default, rename = "candidateCount")]
    candidate_count: Option<u64>,
    /// Refused: the canonical model has no reasoning budget.
    #[serde(default, rename = "thinkingConfig")]
    thinking_config: Option<RawJson>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

/// The schema a function declaration carries when the caller wrote none.
fn empty_object() -> RawJson {
    RawJson::new(serde_json::json!({}))
}
