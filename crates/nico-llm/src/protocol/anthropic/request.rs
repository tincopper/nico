//! Decoding a Messages request body into the canonical request.
//!
//! The shape this codec has to recover is *not* the canonical one: the system prompt is a top-level
//! field, a tool call is a content block of an assistant message and a tool result is a block of a
//! user message. Decoding therefore flushes a run of text/image blocks into one message and lifts
//! every call, result and thinking block out as its own item, in wire order — that is what keeps a
//! conversation that interleaves reasoning, text and calls readable by an adapter that stores them in
//! another order.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::{blocks, field, invalid, reject_extra, schema, unsupported};
use crate::{
    protocol::CodecError,
    schema::{
        ChatRequest, ContentPart, Extensions, Item, Message, ModelId, RawJson, ResponseFormat,
        Role, Sampling, Tool, ToolChoice, ToolName,
    },
};

/// Keys this codec knows and cannot carry.
///
/// Each one changes what the model produces (a thinking budget, a sampling cut-off, a routing tier, a
/// server-side tool or container), so forwarding nothing and answering `200` would tell the caller an
/// instruction took effect when it did not. A key this list does not name is preserved in
/// [`ChatRequest::extensions`] instead.
const REFUSED: [&str; 6] = [
    "thinking",
    "top_k",
    "service_tier",
    "mcp_servers",
    "container",
    "context_management",
];

/// Decodes one buffered request body.
pub(super) fn decode(body: &[u8]) -> Result<ChatRequest, CodecError> {
    let wire: RequestWire = serde_json::from_slice(body)
        .map_err(|error| invalid(format!("the request body is not a valid document: {error}")))?;

    let model = ModelId::try_new(wire.model.clone()).map_err(|error| field("model", error))?;
    let conversation = conversation(&wire)?;
    let sampling = sampling(&wire)?;
    let tools = tools(&wire)?;
    let tool_choice = tool_choice(wire.tool_choice.as_ref())?;
    let extensions = extensions(&wire)?;

    let mut request = ChatRequest::try_new(model, conversation).map_err(schema)?;
    request = request.with_sampling(sampling).with_extensions(extensions);
    if !tools.is_empty() {
        request = request.with_tools(tools).map_err(schema)?;
    }
    request = request.with_tool_choice(tool_choice).map_err(schema)?;
    // This protocol has no JSON mode, so a request cannot ask for one; the field stays at its
    // canonical default and a caller that needs structured output is refused by the capability gate.
    request = request.with_response_format(ResponseFormat::Text);
    Ok(request.with_stream(wire.stream))
}

/// Builds the canonical conversation: the system prompt, then every message's items in wire order.
fn conversation(wire: &RequestWire) -> Result<Vec<Item>, CodecError> {
    if wire.messages.is_empty() {
        return Err(invalid("`messages` must carry at least one message"));
    }
    let mut conversation = Vec::with_capacity(wire.messages.len() + 1);
    if let Some(system) = &wire.system {
        let parts = blocks::system_parts(system, "system")?;
        conversation.push(Item::Message(
            Message::try_new(Role::System, parts).map_err(schema)?,
        ));
    }
    for (index, message) in wire.messages.iter().enumerate() {
        let path = format!("messages[{index}]");
        let role = role(&message.role, &path)?;
        let content = message
            .content
            .as_ref()
            .ok_or_else(|| invalid(format!("`{path}.content` is missing")))?;
        let mut parts: Vec<ContentPart> = Vec::new();
        let mut error: Option<CodecError> = None;
        let decoded = blocks::decode_content(content, &format!("{path}.content"), |block| {
            if error.is_some() {
                return;
            }
            match block {
                blocks::Decoded::Part(part) => parts.push(part),
                blocks::Decoded::Item(item) => {
                    // The run before this item is a message of its own: flushing here is what keeps
                    // the wire order, because the canonical conversation has no nesting.
                    if !parts.is_empty() {
                        match Message::try_new(role, std::mem::take(&mut parts)) {
                            Ok(message) => conversation.push(Item::Message(message)),
                            Err(failure) => error = Some(schema(failure)),
                        }
                    }
                    conversation.push(item);
                }
            }
        });
        if let Some(error) = error {
            return Err(error);
        }
        decoded?;
        if parts.is_empty() {
            // A message that carried only calls, results or reasoning contributes no text run.
            continue;
        }
        conversation.push(Item::Message(
            Message::try_new(role, parts).map_err(schema)?,
        ));
    }
    Ok(conversation)
}

/// Returns the canonical role of one wire role.
///
/// A tool's answer is a `tool_result` block, so this protocol has no `tool` role to translate; a role
/// it does not define is a malformed document.
fn role(value: &str, path: &str) -> Result<Role, CodecError> {
    match value {
        "user" => Ok(Role::User),
        "assistant" => Ok(Role::Assistant),
        other => Err(invalid(format!(
            "`{path}.role` is not a role this protocol defines: `{other}`"
        ))),
    }
}

/// Builds the sampling knobs, refusing a value outside this protocol's own range.
///
/// The canonical range is the widest of the protocols a route can reach, so this codec is where a
/// narrower one is enforced. A value is refused rather than clamped: quietly lowering a temperature
/// changes the answer the caller asked for.
fn sampling(wire: &RequestWire) -> Result<Sampling, CodecError> {
    let max_tokens = wire
        .max_tokens
        .ok_or_else(|| invalid("field `max_tokens` is required by this protocol"))?;
    let max_tokens = u32::try_from(max_tokens)
        .map_err(|_| invalid("field `max_tokens` does not fit this gateway's output ceiling"))?;
    if let Some(temperature) = wire.temperature
        && !(0.0..=1.0).contains(&temperature)
    {
        return Err(invalid(
            "field `temperature` is outside this protocol's range of 0.0 to 1.0",
        ));
    }
    let sampling = Sampling::try_new(wire.temperature, wire.top_p, Some(max_tokens))
        .map_err(|error| field("sampling", error))?;
    if wire.stop_sequences.is_empty() {
        return Ok(sampling);
    }
    sampling
        .with_stop(wire.stop_sequences.clone())
        .map_err(|error| field("stop_sequences", error))
}

/// Builds the declared tools.
fn tools(wire: &RequestWire) -> Result<Vec<Tool>, CodecError> {
    let mut tools = Vec::with_capacity(wire.tools.len());
    for (index, tool) in wire.tools.iter().enumerate() {
        let path = format!("tools[{index}]");
        reject_extra(&tool.extra, &path)?;
        let name = ToolName::try_new(tool.name.clone()).map_err(|error| field(&path, error))?;
        let mut declared =
            Tool::try_new(name, tool.input_schema.clone()).map_err(|error| field(&path, error))?;
        if let Some(description) = &tool.description {
            declared = declared
                .with_description(description.clone())
                .map_err(|error| field(&format!("{path}.description"), error))?;
        }
        tools.push(declared);
    }
    Ok(tools)
}

/// Builds the tool choice.
fn tool_choice(wire: Option<&ToolChoiceWire>) -> Result<ToolChoice, CodecError> {
    let Some(wire) = wire else {
        return Ok(ToolChoice::Auto);
    };
    if !wire.extra.is_empty() {
        // `disable_parallel_tool_use` is the member this protocol adds, and it changes what the model
        // may do; the canonical model has no field for it.
        let member = wire
            .extra
            .keys()
            .next()
            .map_or(String::from("tool_choice"), |key| {
                format!("tool_choice.{key}")
            });
        return Err(unsupported(member));
    }
    match wire.kind.as_str() {
        "auto" => Ok(ToolChoice::Auto),
        "any" => Ok(ToolChoice::Required),
        "none" => Ok(ToolChoice::None),
        "tool" => {
            let name = wire
                .name
                .as_ref()
                .ok_or_else(|| invalid("`tool_choice.name` is required when `type` is `tool`"))?;
            Ok(ToolChoice::Tool(
                ToolName::try_new(name.clone())
                    .map_err(|error| field("tool_choice.name", error))?,
            ))
        }
        other => Err(invalid(format!(
            "`tool_choice.type` is not a value this protocol defines: `{other}`"
        ))),
    }
}

/// Collects the fields this codec preserves, refusing the ones it knows it cannot carry.
fn extensions(wire: &RequestWire) -> Result<Extensions, CodecError> {
    let mut extensions = Extensions::new();
    if let Some(metadata) = &wire.metadata {
        extensions
            .try_insert("metadata", metadata.clone())
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

/// One request body.
#[derive(Deserialize)]
struct RequestWire {
    model: String,
    #[serde(default)]
    system: Option<RawJson>,
    messages: Vec<MessageWire>,
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    top_p: Option<f64>,
    #[serde(default)]
    stop_sequences: Vec<String>,
    #[serde(default)]
    tools: Vec<ToolWire>,
    #[serde(default)]
    tool_choice: Option<ToolChoiceWire>,
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    metadata: Option<RawJson>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

/// One message of a request.
#[derive(Deserialize)]
struct MessageWire {
    role: String,
    #[serde(default)]
    content: Option<RawJson>,
}

/// One declared tool.
#[derive(Deserialize)]
struct ToolWire {
    name: String,
    #[serde(default)]
    description: Option<String>,
    input_schema: RawJson,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

/// How the model may choose a tool.
#[derive(Deserialize)]
struct ToolChoiceWire {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}
