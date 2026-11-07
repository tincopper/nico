//! Decoding a Responses request body into the canonical request.
//!
//! This protocol separates what the others fold into messages: `instructions` is the system prompt,
//! `input` is the conversation as items, and a tool call or result is an item of its own rather than a
//! block inside a message. Decoding is therefore a straight walk: the instructions become one leading
//! system item, and every input item becomes exactly one canonical item.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::{field, invalid, items, schema, unsupported};
use crate::{
    protocol::CodecError,
    schema::{
        ChatRequest, Extensions, Item, Message, ModelId, RawJson, ResponseFormat, Role, Sampling,
        Tool, ToolChoice, ToolName,
    },
};

/// Keys this codec knows and cannot carry.
///
/// Every one of them changes what the model produces or how the provider retains it: a stateful
/// continuation, a storage opt-in, a reasoning budget, a built-in tool, a truncation policy, a routing
/// tier, a background run, a prompt template, or a sampling knob this protocol does not have. A key this
/// list does not name is preserved in [`ChatRequest::extensions`] instead.
const REFUSED: [&str; 12] = [
    "previous_response_id",
    "reasoning",
    "truncation",
    "parallel_tool_calls",
    "include",
    "service_tier",
    "background",
    "prompt",
    "stop",
    "seed",
    "presence_penalty",
    "frequency_penalty",
];

/// Decodes one buffered request body.
pub(super) fn decode(body: &[u8]) -> Result<ChatRequest, CodecError> {
    let wire: RequestWire = serde_json::from_slice(body)
        .map_err(|error| invalid(format!("the request body is not a valid document: {error}")))?;

    if wire.store == Some(true) {
        return Err(unsupported("store"));
    }
    let model = ModelId::try_new(wire.model.clone()).map_err(|error| field("model", error))?;
    let conversation = conversation(&wire)?;
    let sampling = sampling(&wire)?;
    let tools = tools(&wire)?;
    let tool_choice = tool_choice(wire.tool_choice.as_ref())?;
    let response_format = response_format(wire.text.as_ref())?;
    let extensions = extensions(&wire)?;

    let mut request = ChatRequest::try_new(model, conversation)
        .map_err(schema)?
        .with_sampling(sampling)
        .with_response_format(response_format)
        .with_extensions(extensions);
    if !tools.is_empty() {
        request = request.with_tools(tools).map_err(schema)?;
    }
    request = request.with_tool_choice(tool_choice).map_err(schema)?;
    Ok(request.with_stream(wire.stream))
}

/// Builds the canonical conversation: the instructions, then every input item.
fn conversation(wire: &RequestWire) -> Result<Vec<Item>, CodecError> {
    let mut conversation = Vec::new();
    if let Some(instructions) = &wire.instructions {
        if instructions.trim().is_empty() {
            return Err(invalid("`instructions` is present but empty"));
        }
        conversation.push(Item::Message(
            Message::try_new(
                Role::System,
                vec![crate::schema::ContentPart::text(instructions)],
            )
            .map_err(schema)?,
        ));
    }
    let input = wire
        .input
        .as_ref()
        .ok_or_else(|| invalid("field `input` is required by this protocol"))?;
    match input.as_value() {
        serde_json::Value::String(text) => conversation.push(Item::Message(Message::user(text))),
        serde_json::Value::Array(raw) => {
            let mut leading = conversation.is_empty();
            for (index, item) in raw.iter().enumerate() {
                let path = format!("input[{index}]");
                match items::decode_item(&RawJson::new(item.clone()), &path)? {
                    items::Decoded::Message(message) => {
                        let system = matches!(message.role(), Role::System | Role::Developer);
                        if system && !leading {
                            return Err(unsupported(format!("{path}.role")));
                        }
                        leading = system;
                        conversation.push(Item::Message(message));
                    }
                    items::Decoded::Item(item) => {
                        leading = false;
                        conversation.push(item);
                    }
                }
            }
        }
        _ => {
            return Err(invalid("`input` must be a string or an array of items"));
        }
    }
    if conversation.is_empty() {
        return Err(invalid("`input` carries nothing"));
    }
    Ok(conversation)
}

/// Builds the sampling knobs.
fn sampling(wire: &RequestWire) -> Result<Sampling, CodecError> {
    let max_tokens = match wire.max_output_tokens {
        Some(value) => Some(
            u32::try_from(value)
                .map_err(|_| invalid("field `max_output_tokens` does not fit this gateway"))?,
        ),
        None => None,
    };
    Sampling::try_new(wire.temperature, wire.top_p, max_tokens)
        .map_err(|error| field("sampling", error))
}

/// Builds the declared tools.
fn tools(wire: &RequestWire) -> Result<Vec<Tool>, CodecError> {
    let mut tools = Vec::with_capacity(wire.tools.len());
    for (index, raw) in wire.tools.iter().enumerate() {
        let path = format!("tools[{index}]");
        // The type is read before the typed decode: a built-in tool carries different members, and the
        // refusal must name the type rather than complain about a missing function field.
        let kind = raw
            .as_value()
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if kind != "function" {
            // The built-in tools run at the provider; the gateway neither calls them nor can replay
            // their results, so a declaration for one is a field this route cannot serve.
            return Err(unsupported(format!("{path}.type")));
        }
        let tool: ToolIn = serde_json::from_value(raw.as_value().clone())
            .map_err(|error| invalid(format!("field `{path}` is invalid: {error}")))?;
        if let Some(key) = tool.extra.keys().next() {
            return Err(unsupported(format!("{path}.{key}")));
        }
        let name = ToolName::try_new(tool.name).map_err(|error| field(&path, error))?;
        let mut declared =
            Tool::try_new(name, tool.parameters).map_err(|error| field(&path, error))?;
        if let Some(description) = tool.description {
            declared = declared
                .with_description(description)
                .map_err(|error| field(&format!("{path}.description"), error))?;
        }
        tools.push(declared);
    }
    Ok(tools)
}

/// Builds the tool choice.
fn tool_choice(wire: Option<&RawJson>) -> Result<ToolChoice, CodecError> {
    let Some(wire) = wire else {
        return Ok(ToolChoice::Auto);
    };
    match wire.as_value() {
        serde_json::Value::String(mode) => match mode.as_str() {
            "auto" => Ok(ToolChoice::Auto),
            "none" => Ok(ToolChoice::None),
            "required" => Ok(ToolChoice::Required),
            other => Err(invalid(format!(
                "`tool_choice` is not a mode this protocol defines: `{other}`"
            ))),
        },
        serde_json::Value::Object(_) => {
            let named: NamedToolIn = serde_json::from_value(wire.as_value().clone())
                .map_err(|error| invalid(format!("field `tool_choice` is invalid: {error}")))?;
            if let Some(key) = named.extra.keys().next() {
                return Err(unsupported(format!("tool_choice.{key}")));
            }
            Ok(ToolChoice::Tool(
                ToolName::try_new(named.name).map_err(|error| field("tool_choice.name", error))?,
            ))
        }
        _ => Err(invalid(
            "`tool_choice` must be a mode name or a named function",
        )),
    }
}

/// Builds the structured-output request.
fn response_format(wire: Option<&RawJson>) -> Result<ResponseFormat, CodecError> {
    let Some(text) = wire else {
        return Ok(ResponseFormat::Text);
    };
    let text: TextIn = serde_json::from_value(text.as_value().clone())
        .map_err(|error| invalid(format!("field `text` is invalid: {error}")))?;
    if !text.extra.is_empty() {
        let key = text
            .extra
            .keys()
            .next()
            .map_or(String::from("text"), |key| format!("text.{key}"));
        return Err(unsupported(key));
    }
    let Some(format) = text.format else {
        return Ok(ResponseFormat::Text);
    };
    if let Some(key) = format.extra.keys().next() {
        return Err(unsupported(format!("text.format.{key}")));
    }
    match format._kind.as_str() {
        "text" => Ok(ResponseFormat::Text),
        "json_object" => Ok(ResponseFormat::JsonObject),
        "json_schema" => Ok(ResponseFormat::JsonSchema(
            crate::schema::JsonSchema::try_new(
                format.name,
                format.schema.unwrap_or_else(empty_object),
            )
            .map_err(schema)?,
        )),
        other => Err(invalid(format!(
            "`text.format.type` is not a value this protocol defines: `{other}`"
        ))),
    }
}

/// The schema a `json_schema` format carries when the caller wrote none.
///
/// This protocol requires a schema for that format; a request without one is a malformed document, and
/// the canonical constructor refuses a non-object schema, so the empty object is the value that keeps
/// the refusal about the missing member rather than about its type.
fn empty_object() -> RawJson {
    RawJson::new(serde_json::Value::Object(serde_json::Map::new()))
}

/// Collects the fields this codec preserves, refusing the ones it knows it cannot carry.
fn extensions(wire: &RequestWire) -> Result<Extensions, CodecError> {
    let mut extensions = Extensions::new();
    for (name, value) in [("metadata", &wire.metadata), ("user", &wire.user)] {
        if let Some(value) = value {
            extensions.try_insert(name, value.clone()).map_err(schema)?;
        }
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

/// One request body.
#[derive(Deserialize)]
struct RequestWire {
    model: String,
    #[serde(default)]
    instructions: Option<String>,
    #[serde(default)]
    input: Option<RawJson>,
    #[serde(default)]
    max_output_tokens: Option<u64>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    top_p: Option<f64>,
    #[serde(default)]
    tools: Vec<RawJson>,
    #[serde(default)]
    tool_choice: Option<RawJson>,
    #[serde(default)]
    text: Option<RawJson>,
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    store: Option<bool>,
    #[serde(default)]
    metadata: Option<RawJson>,
    #[serde(default)]
    user: Option<RawJson>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

/// One declared tool.
#[derive(Deserialize)]
struct ToolIn {
    /// The item's discriminator, which the dispatch above has already read.
    #[serde(rename = "type")]
    _kind: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default = "empty_object")]
    parameters: RawJson,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

/// One named tool choice.
#[derive(Deserialize)]
struct NamedToolIn {
    /// The item's discriminator, which the dispatch above has already read.
    #[serde(rename = "type")]
    _kind: String,
    name: String,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

/// The `text` object of a request.
#[derive(Deserialize)]
struct TextIn {
    #[serde(default)]
    format: Option<FormatIn>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

/// The `text.format` object of a request.
#[derive(Deserialize)]
struct FormatIn {
    /// The item's discriminator, which the dispatch above has already read.
    #[serde(rename = "type")]
    _kind: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    schema: Option<RawJson>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}
