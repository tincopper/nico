//! What the caller declares about the answer: the tools the model may call, how it may choose one,
//! and the shape the answer must take.
//!
//! The declarations are decoded apart from the conversation because they carry the caller's JSON
//! documents (a tool's parameter schema, the structured-output schema) and their own closed member
//! table: a member this gateway cannot carry is refused by name instead of being forwarded into a
//! provider that would silently ignore it.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::{field, invalid, reject_extra, schema, unsupported};
use crate::{
    protocol::CodecError,
    schema::{JsonSchema, RawJson, ResponseFormat, Tool, ToolChoice, ToolName},
};

/// One `tools[]` declaration.
#[derive(Deserialize)]
pub(super) struct ToolWire {
    #[serde(rename = "type")]
    kind: Option<String>,
    function: FunctionWire,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

/// A function declaration.
#[derive(Deserialize)]
struct FunctionWire {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    parameters: Option<RawJson>,
    #[serde(default)]
    strict: Option<bool>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

impl ToolWire {
    /// Decodes one declaration.
    fn decode(self, index: usize) -> Result<Tool, CodecError> {
        let path = format!("tools[{index}]");
        reject_extra(&self.extra, &path)?;
        if !matches!(self.kind.as_deref(), None | Some("function")) {
            return Err(unsupported(format!("{path}.type")));
        }
        reject_extra(&self.function.extra, &format!("{path}.function"))?;
        if self.function.strict.is_some() {
            return Err(unsupported(format!("{path}.function.strict")));
        }
        let name = ToolName::try_new(self.function.name)
            .map_err(|error| field(&format!("{path}.function.name"), error))?;
        let parameters = self.function.parameters.unwrap_or_else(empty_object);
        let tool = Tool::try_new(name, parameters).map_err(schema)?;
        match self.function.description {
            Some(description) => tool.with_description(description).map_err(schema),
            None => Ok(tool),
        }
    }
}

/// How the model may choose a tool, as the wire spells it: a mode string or a function reference.
#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum ToolChoiceWire {
    Mode(String),
    Named(NamedToolChoiceWire),
}

/// A reference to one declared function.
#[derive(Deserialize)]
pub(super) struct NamedToolChoiceWire {
    #[serde(rename = "type")]
    kind: String,
    function: NamedFunctionWire,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

/// The name inside a function reference.
#[derive(Deserialize)]
pub(super) struct NamedFunctionWire {
    name: String,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

impl ToolChoiceWire {
    /// Maps one tool choice.
    pub(super) fn decode(&self) -> Result<ToolChoice, CodecError> {
        match self {
            Self::Mode(mode) => match mode.as_str() {
                "none" => Ok(ToolChoice::None),
                "auto" => Ok(ToolChoice::Auto),
                "required" => Ok(ToolChoice::Required),
                _ => Err(unsupported("tool_choice")),
            },
            Self::Named(named) => {
                reject_extra(&named.extra, "tool_choice")?;
                reject_extra(&named.function.extra, "tool_choice.function")?;
                if named.kind != "function" {
                    return Err(unsupported("tool_choice.type"));
                }
                let name = ToolName::try_new(named.function.name.clone())
                    .map_err(|error| field("tool_choice.function.name", error))?;
                Ok(ToolChoice::Tool(name))
            }
        }
    }
}

/// The response-format request.
#[derive(Deserialize)]
pub(super) struct ResponseFormatWire {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    json_schema: Option<JsonSchemaWire>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

/// The JSON-Schema contract of a structured response.
#[derive(Deserialize)]
struct JsonSchemaWire {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    strict: Option<bool>,
    #[serde(default)]
    schema: Option<RawJson>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

impl ResponseFormatWire {
    /// Maps one response format.
    pub(super) fn decode(&self) -> Result<ResponseFormat, CodecError> {
        reject_extra(&self.extra, "response_format")?;
        match self.kind.as_str() {
            "text" => {
                self.reject_schema()?;
                Ok(ResponseFormat::Text)
            }
            "json_object" => {
                self.reject_schema()?;
                Ok(ResponseFormat::JsonObject)
            }
            "json_schema" => {
                let schema = self.json_schema.as_ref().ok_or_else(|| {
                    invalid(
                        "field `response_format.json_schema` is required for type `json_schema`",
                    )
                })?;
                Ok(ResponseFormat::JsonSchema(schema.decode()?))
            }
            _ => Err(unsupported("response_format.type")),
        }
    }

    /// Refuses a schema on a format that does not take one.
    fn reject_schema(&self) -> Result<(), CodecError> {
        if self.json_schema.is_some() {
            return Err(invalid(
                "field `response_format.json_schema` is only valid for type `json_schema`",
            ));
        }
        Ok(())
    }
}

impl JsonSchemaWire {
    /// Maps one JSON-Schema contract.
    fn decode(&self) -> Result<JsonSchema, CodecError> {
        reject_extra(&self.extra, "response_format.json_schema")?;
        if self.description.is_some() {
            return Err(unsupported("response_format.json_schema.description"));
        }
        if self.strict.is_some() {
            return Err(unsupported("response_format.json_schema.strict"));
        }
        let document = self
            .schema
            .clone()
            .ok_or_else(|| invalid("field `response_format.json_schema.schema` is required"))?;
        JsonSchema::try_new(self.name.clone(), document).map_err(schema)
    }
}

/// Decodes every declaration, keeping the wire order.
pub(super) fn decode_tools(tools: Option<Vec<ToolWire>>) -> Result<Vec<Tool>, CodecError> {
    let Some(tools) = tools else {
        return Ok(Vec::new());
    };
    tools
        .into_iter()
        .enumerate()
        .map(|(index, tool)| tool.decode(index))
        .collect()
}

/// Returns the empty JSON object a declaration without parameters means.
fn empty_object() -> RawJson {
    RawJson::new(serde_json::Value::Object(serde_json::Map::new()))
}
