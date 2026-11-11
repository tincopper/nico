//! The canonical chat request, its response format and the JSON-Schema contract of a structured
//! response.
//!
//! [`ChatRequest`] is what a client codec produces and what a provider adapter consumes. It carries
//! the *logical* model the client wrote; resolving that to a physical model belongs to the route's
//! provider map, not to the request shape. Everything the model does not name is carried in
//! [`Extensions`] rather than dropped.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::{
    Extensions, Item, ModelId, RawJson, Sampling, SchemaError, Tool, ToolChoice, error::excerpt,
};

/// A request to produce one model turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    model: ModelId,
    conversation: Vec<Item>,
    #[serde(default)]
    tools: Vec<Tool>,
    #[serde(default)]
    tool_choice: ToolChoice,
    #[serde(default)]
    sampling: Sampling,
    #[serde(default)]
    response_format: ResponseFormat,
    #[serde(default)]
    stream: bool,
    #[serde(flatten)]
    extensions: Extensions,
}

impl ChatRequest {
    /// Builds a request with default sampling, no tools and a text response.
    ///
    /// # Errors
    /// Returns [`SchemaError::EmptyConversation`] when `conversation` is empty, or the validation
    /// error of an item.
    pub fn try_new(model: ModelId, conversation: Vec<Item>) -> Result<Self, SchemaError> {
        let request = Self {
            model,
            conversation,
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            sampling: Sampling::default(),
            response_format: ResponseFormat::Text,
            stream: false,
            extensions: Extensions::new(),
        };
        request.validate()?;
        Ok(request)
    }

    /// Replaces the declared tools.
    ///
    /// # Errors
    /// Returns the validation error of a tool (an empty name, a description that is only
    /// whitespace, or parameters that are not a JSON object), [`SchemaError::Duplicate`] when two
    /// tools share a name, or [`SchemaError::UndeclaredTool`] when an earlier [`ToolChoice::Tool`]
    /// names a tool this set does not contain.
    pub fn with_tools(mut self, tools: Vec<Tool>) -> Result<Self, SchemaError> {
        validate_tools(&tools, &self.tool_choice)?;
        self.tools = tools;
        Ok(self)
    }

    /// Sets how the model may choose a tool.
    ///
    /// # Errors
    /// Returns [`SchemaError::UndeclaredTool`] when a named choice is not declared in `tools`.
    pub fn with_tool_choice(mut self, tool_choice: ToolChoice) -> Result<Self, SchemaError> {
        validate_tools(&self.tools, &tool_choice)?;
        self.tool_choice = tool_choice;
        Ok(self)
    }

    /// Replaces the sampling knobs. A [`Sampling`] value is already validated.
    #[must_use]
    pub fn with_sampling(mut self, sampling: Sampling) -> Self {
        self.sampling = sampling;
        self
    }

    /// Replaces the response format. A [`ResponseFormat`] value is already validated.
    #[must_use]
    pub fn with_response_format(mut self, response_format: ResponseFormat) -> Self {
        self.response_format = response_format;
        self
    }

    /// Marks the request as a streaming one.
    #[must_use]
    pub fn with_stream(mut self, stream: bool) -> Self {
        self.stream = stream;
        self
    }

    /// Replaces the unmodelled fields.
    ///
    /// Keys are checked against the modelled field names by [`ChatRequest::validate`], not here:
    /// whether a key shadows a field depends on the whole shape, so a request built with this
    /// method must be validated before it is used.
    #[must_use]
    pub fn with_extensions(mut self, extensions: Extensions) -> Self {
        self.extensions = extensions;
        self
    }

    /// Checks the whole request tree.
    ///
    /// Call this after deserializing a request from a fixture or diagnostic representation; a
    /// constructor already validated what it built.
    ///
    /// # Errors
    /// Returns the first broken invariant: an empty conversation, an invalid item or sampling
    /// value, an undeclared or duplicated tool, an invalid response format, an invalid extension
    /// key, or an extension key that shadows a modelled field.
    pub fn validate(&self) -> Result<(), SchemaError> {
        if self.conversation.is_empty() {
            return Err(SchemaError::EmptyConversation);
        }
        for item in &self.conversation {
            item.validate()?;
        }
        validate_tools(&self.tools, &self.tool_choice)?;
        self.sampling.validate()?;
        self.response_format.validate()?;
        self.extensions.validate()?;
        self.extensions.reject_modelled_keys(&[
            "model",
            "conversation",
            "tools",
            "tool_choice",
            "sampling",
            "response_format",
            "stream",
        ])
    }

    /// Returns the logical model the client wrote.
    #[must_use]
    pub const fn model(&self) -> &ModelId {
        &self.model
    }

    /// Returns the conversation items in order.
    #[must_use]
    pub fn conversation(&self) -> &[Item] {
        &self.conversation
    }

    /// Returns the declared tools.
    #[must_use]
    pub fn tools(&self) -> &[Tool] {
        &self.tools
    }

    /// Returns how the model may choose a tool.
    #[must_use]
    pub const fn tool_choice(&self) -> &ToolChoice {
        &self.tool_choice
    }

    /// Returns the sampling knobs.
    #[must_use]
    pub const fn sampling(&self) -> &Sampling {
        &self.sampling
    }

    /// Returns the response format.
    #[must_use]
    pub const fn response_format(&self) -> &ResponseFormat {
        &self.response_format
    }

    /// Reports whether the client asked for a stream.
    #[must_use]
    pub const fn stream(&self) -> bool {
        self.stream
    }

    /// Returns the fields the canonical model does not name.
    #[must_use]
    pub const fn extensions(&self) -> &Extensions {
        &self.extensions
    }
}

/// A JSON Schema a structured response must satisfy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "JsonSchemaWire")]
pub struct JsonSchema {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    schema: RawJson,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonSchemaWire {
    #[serde(default)]
    name: Option<String>,
    schema: RawJson,
}

impl JsonSchema {
    /// Builds a JSON-Schema response contract.
    ///
    /// # Errors
    /// Returns [`SchemaError::Empty`] for an empty name, [`SchemaError::NotAnObject`] when the
    /// schema is not an object, and [`SchemaError::EmptyObject`] when it is an empty object.
    pub fn try_new(name: Option<String>, schema: RawJson) -> Result<Self, SchemaError> {
        let value = Self { name, schema };
        value.validate()?;
        Ok(value)
    }

    /// Checks the name and schema.
    ///
    /// # Errors
    /// Returns the same variants as [`JsonSchema::try_new`].
    pub fn validate(&self) -> Result<(), SchemaError> {
        if let Some(name) = &self.name
            && name.trim().is_empty()
        {
            return Err(SchemaError::Empty {
                field: "response_format.json_schema.name",
            });
        }
        if !self.schema.is_object() {
            return Err(SchemaError::NotAnObject {
                field: "response_format.json_schema",
            });
        }
        if self.schema.is_empty_object() {
            return Err(SchemaError::EmptyObject {
                field: "response_format.json_schema",
            });
        }
        Ok(())
    }

    /// Returns the optional schema name a provider may use to identify it.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Returns the schema document.
    #[must_use]
    pub const fn schema(&self) -> &RawJson {
        &self.schema
    }
}

impl TryFrom<JsonSchemaWire> for JsonSchema {
    type Error = SchemaError;

    fn try_from(wire: JsonSchemaWire) -> Result<Self, Self::Error> {
        Self::try_new(wire.name, wire.schema)
    }
}

/// The shape the response body must take.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ResponseFormat {
    /// Free-form text.
    #[default]
    Text,
    /// Any valid JSON object.
    JsonObject,
    /// A JSON object satisfying the given schema.
    JsonSchema(JsonSchema),
}

impl ResponseFormat {
    /// Checks the schema when this format carries one.
    ///
    /// # Errors
    /// Returns [`SchemaError::NotAnObject`] or [`SchemaError::EmptyObject`] for an invalid schema,
    /// or [`SchemaError::Empty`] for an empty name.
    pub fn validate(&self) -> Result<(), SchemaError> {
        match self {
            Self::JsonSchema(schema) => schema.validate(),
            Self::Text | Self::JsonObject => Ok(()),
        }
    }
}

fn validate_tools(tools: &[Tool], choice: &ToolChoice) -> Result<(), SchemaError> {
    let mut seen = BTreeSet::new();
    for tool in tools {
        tool.validate()?;
        if !seen.insert(tool.name().as_str()) {
            return Err(SchemaError::Duplicate {
                kind: "tool",
                value: excerpt(tool.name().as_str()),
            });
        }
    }
    if let ToolChoice::Tool(name) = choice
        && !tools.iter().any(|tool| tool.name() == name)
    {
        return Err(SchemaError::UndeclaredTool {
            name: excerpt(name.as_str()),
        });
    }
    Ok(())
}
