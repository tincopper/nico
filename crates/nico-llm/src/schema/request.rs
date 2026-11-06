//! The canonical chat request, its sampling knobs and its response format.
//!
//! [`ChatRequest`] is what a client codec produces and what a provider adapter consumes. It carries
//! the *logical* model the client wrote; resolving that to a physical model belongs to the route's
//! provider map, not to the request shape. Everything the model does not name is carried in
//! [`Extensions`] rather than dropped.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::{Extensions, Item, ModelId, RawJson, SchemaError, Tool, ToolChoice, error::excerpt};

/// Highest `temperature` accepted; the range every target protocol fits.
const MAX_TEMPERATURE: f64 = 2.0;
/// Lowest `top_p` accepted.
const MIN_TOP_P: f64 = 0.0;
/// Highest `top_p` accepted.
const MAX_TOP_P: f64 = 1.0;

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

    /// Returns the sampling knobs (a small `Copy` value).
    #[must_use]
    pub const fn sampling(&self) -> Sampling {
        self.sampling
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

/// Sampling knobs, all optional so "unset" stays distinct from "explicitly zero".
///
/// The bounds are the narrowest common denominator of the target protocols: a value accepted here
/// can be encoded by every one of them, so a range failure is reported once, at construction,
/// instead of surfacing as a provider 400.
///
/// The floats are `f64` because that is what a JSON number is: storing `f32` would make a canonical
/// round trip lossy (`0.2` would serialize back as `0.20000000298023224`) and a golden fixture could
/// not compare equal. A sampling knob the canonical model does not name is rejected rather than
/// dropped: `Sampling` has no extension bucket, and silently forwarding an unknown knob would let
/// a caller believe it took effect.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "SamplingWire")]
pub struct Sampling {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SamplingWire {
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    top_p: Option<f64>,
    #[serde(default)]
    max_tokens: Option<u32>,
}

impl Sampling {
    /// Builds sampling knobs.
    ///
    /// # Errors
    /// Returns [`SchemaError::NotFinite`] or [`SchemaError::OutOfRange`] for a non-finite or
    /// out-of-range `temperature`/`top_p`, and [`SchemaError::OutOfRange`] for `max_tokens == 0`.
    pub fn try_new(
        temperature: Option<f64>,
        top_p: Option<f64>,
        max_tokens: Option<u32>,
    ) -> Result<Self, SchemaError> {
        let sampling = Self {
            temperature,
            top_p,
            max_tokens,
        };
        sampling.validate()?;
        Ok(sampling)
    }

    /// Sets `temperature`, which controls randomness.
    ///
    /// # Errors
    /// Returns [`SchemaError::NotFinite`] or [`SchemaError::OutOfRange`] when the value is not in
    /// `0.0..=2.0`.
    pub fn with_temperature(mut self, temperature: f64) -> Result<Self, SchemaError> {
        validate_temperature(temperature)?;
        self.temperature = Some(temperature);
        Ok(self)
    }

    /// Sets `top_p`, which restricts sampling to the most likely tokens.
    ///
    /// # Errors
    /// Returns [`SchemaError::NotFinite`] or [`SchemaError::OutOfRange`] when the value is not in
    /// `0.0..=1.0`.
    pub fn with_top_p(mut self, top_p: f64) -> Result<Self, SchemaError> {
        validate_top_p(top_p)?;
        self.top_p = Some(top_p);
        Ok(self)
    }

    /// Sets the output token ceiling.
    ///
    /// # Errors
    /// Returns [`SchemaError::OutOfRange`] when `max_tokens` is zero.
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Result<Self, SchemaError> {
        validate_max_tokens(max_tokens)?;
        self.max_tokens = Some(max_tokens);
        Ok(self)
    }

    /// Checks every set knob.
    ///
    /// # Errors
    /// Returns the same variants as [`Sampling::try_new`].
    pub fn validate(&self) -> Result<(), SchemaError> {
        if let Some(temperature) = self.temperature {
            validate_temperature(temperature)?;
        }
        if let Some(top_p) = self.top_p {
            validate_top_p(top_p)?;
        }
        if let Some(max_tokens) = self.max_tokens {
            validate_max_tokens(max_tokens)?;
        }
        Ok(())
    }

    /// Returns `temperature`.
    #[must_use]
    pub const fn temperature(&self) -> Option<f64> {
        self.temperature
    }

    /// Returns `top_p`.
    #[must_use]
    pub const fn top_p(&self) -> Option<f64> {
        self.top_p
    }

    /// Returns the output token ceiling.
    #[must_use]
    pub const fn max_tokens(&self) -> Option<u32> {
        self.max_tokens
    }
}

impl TryFrom<SamplingWire> for Sampling {
    type Error = SchemaError;

    fn try_from(wire: SamplingWire) -> Result<Self, Self::Error> {
        Self::try_new(wire.temperature, wire.top_p, wire.max_tokens)
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
        if let Some(name) = &self.name {
            if name.trim().is_empty() {
                return Err(SchemaError::Empty {
                    field: "response_format.json_schema.name",
                });
            }
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

fn validate_temperature(temperature: f64) -> Result<(), SchemaError> {
    if !temperature.is_finite() {
        return Err(SchemaError::NotFinite {
            field: "sampling.temperature",
        });
    }
    if !(0.0..=MAX_TEMPERATURE).contains(&temperature) {
        return Err(SchemaError::OutOfRange {
            field: "sampling.temperature",
            value: temperature.to_string(),
        });
    }
    Ok(())
}

fn validate_top_p(top_p: f64) -> Result<(), SchemaError> {
    if !top_p.is_finite() {
        return Err(SchemaError::NotFinite {
            field: "sampling.top_p",
        });
    }
    if !(MIN_TOP_P..=MAX_TOP_P).contains(&top_p) {
        return Err(SchemaError::OutOfRange {
            field: "sampling.top_p",
            value: top_p.to_string(),
        });
    }
    Ok(())
}

fn validate_max_tokens(max_tokens: u32) -> Result<(), SchemaError> {
    if max_tokens == 0 {
        return Err(SchemaError::OutOfRange {
            field: "sampling.max_tokens",
            value: max_tokens.to_string(),
        });
    }
    Ok(())
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
    if let ToolChoice::Tool(name) = choice {
        if !tools.iter().any(|tool| tool.name() == name) {
            return Err(SchemaError::UndeclaredTool {
                name: excerpt(name.as_str()),
            });
        }
    }
    Ok(())
}
