//! Tools, their arguments, tool results, and how the model may choose one.
//!
//! A tool declaration and a tool call both carry a caller-supplied JSON document whose shape the
//! gateway cannot know; both use [`RawJson`](super::RawJson), the escape hatch documented in
//! [`super::extension`].

use std::fmt;

use serde::{Deserialize, Serialize};

use super::{ContentPart, RawJson, SchemaError};

/// A non-empty tool name.
///
/// Tool names are the address a model uses to call a function, so an empty one can only produce an
/// uncallable declaration.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct ToolName(String);

impl ToolName {
    /// Builds a tool name.
    ///
    /// # Errors
    /// Returns [`SchemaError::Empty`] when the name is empty or only whitespace.
    pub fn try_new(value: impl Into<String>) -> Result<Self, SchemaError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(SchemaError::Empty { field: "tool.name" });
        }
        Ok(Self(value))
    }

    /// Returns the name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ToolName {
    type Error = SchemaError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl fmt::Display for ToolName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A non-empty tool-call identifier.
///
/// A provider uses the id to correlate a call with its result and with later streamed fragments, so
/// an empty id would make the call unaddressable.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct ToolCallId(String);

impl ToolCallId {
    /// Builds a tool-call id.
    ///
    /// # Errors
    /// Returns [`SchemaError::Empty`] when the id is empty or only whitespace.
    pub fn try_new(value: impl Into<String>) -> Result<Self, SchemaError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(SchemaError::Empty {
                field: "tool_call.id",
            });
        }
        Ok(Self(value))
    }

    /// Returns the id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ToolCallId {
    type Error = SchemaError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl fmt::Display for ToolCallId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A function the model may call.
///
/// `parameters` is a JSON Schema document. It must be an object: every provider expects a schema,
/// and an array or scalar there would be sent and rejected upstream instead of failing here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    name: ToolName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    parameters: RawJson,
}

impl Tool {
    /// Declares a tool with no description.
    ///
    /// # Errors
    /// Returns [`SchemaError::NotAnObject`] when `parameters` is not a JSON object.
    pub fn try_new(name: ToolName, parameters: RawJson) -> Result<Self, SchemaError> {
        let tool = Self {
            name,
            description: None,
            parameters,
        };
        tool.validate()?;
        Ok(tool)
    }

    /// Attaches the description a model reads when choosing a tool.
    ///
    /// # Errors
    /// Returns [`SchemaError::Empty`] when the description is empty or only whitespace.
    pub fn with_description(mut self, description: impl Into<String>) -> Result<Self, SchemaError> {
        let description = description.into();
        if description.trim().is_empty() {
            return Err(SchemaError::Empty {
                field: "tool.description",
            });
        }
        self.description = Some(description);
        Ok(self)
    }

    /// Checks the parameters are an object and the description is non-empty.
    ///
    /// # Errors
    /// Returns [`SchemaError::NotAnObject`] or [`SchemaError::Empty`].
    pub fn validate(&self) -> Result<(), SchemaError> {
        if !self.parameters.is_object() {
            return Err(SchemaError::NotAnObject {
                field: "tool.parameters",
            });
        }
        if let Some(description) = &self.description {
            if description.trim().is_empty() {
                return Err(SchemaError::Empty {
                    field: "tool.description",
                });
            }
        }
        Ok(())
    }

    /// Returns the tool name.
    #[must_use]
    pub const fn name(&self) -> &ToolName {
        &self.name
    }

    /// Returns the description, when the declaration has one.
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Returns the JSON Schema of the tool's arguments.
    #[must_use]
    pub const fn parameters(&self) -> &RawJson {
        &self.parameters
    }
}

/// One call the model asked the caller to make.
///
/// Arguments are kept as [`RawJson`] because only the tool knows their schema, but they must still
/// be an object: providers serialize them to a JSON string and clients parse them back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    id: ToolCallId,
    name: ToolName,
    arguments: RawJson,
}

impl ToolCall {
    /// Builds a tool call.
    ///
    /// # Errors
    /// Returns [`SchemaError::NotAnObject`] when `arguments` is not a JSON object.
    pub fn try_new(
        id: ToolCallId,
        name: ToolName,
        arguments: RawJson,
    ) -> Result<Self, SchemaError> {
        let call = Self {
            id,
            name,
            arguments,
        };
        call.validate()?;
        Ok(call)
    }

    /// Checks the arguments are an object.
    ///
    /// # Errors
    /// Returns [`SchemaError::NotAnObject`] when they are not.
    pub fn validate(&self) -> Result<(), SchemaError> {
        if !self.arguments.is_object() {
            return Err(SchemaError::NotAnObject {
                field: "tool_call.arguments",
            });
        }
        Ok(())
    }

    /// Returns the call id.
    #[must_use]
    pub const fn id(&self) -> &ToolCallId {
        &self.id
    }

    /// Returns the called tool's name.
    #[must_use]
    pub const fn name(&self) -> &ToolName {
        &self.name
    }

    /// Returns the arguments the model produced.
    #[must_use]
    pub const fn arguments(&self) -> &RawJson {
        &self.arguments
    }
}

/// What the caller produced for one tool call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    id: ToolCallId,
    content: Vec<ContentPart>,
    #[serde(default, skip_serializing_if = "is_false")]
    is_error: bool,
}

impl ToolResult {
    /// Builds a successful tool result.
    ///
    /// # Errors
    /// Returns [`SchemaError::EmptyContent`] when `content` is empty, or the validation error of a
    /// content part.
    pub fn try_new(id: ToolCallId, content: Vec<ContentPart>) -> Result<Self, SchemaError> {
        let result = Self {
            id,
            content,
            is_error: false,
        };
        result.validate()?;
        Ok(result)
    }

    /// Marks the result as a tool failure rather than a tool answer.
    #[must_use]
    pub fn with_error(mut self, is_error: bool) -> Self {
        self.is_error = is_error;
        self
    }

    /// Checks the result holds at least one valid content part.
    ///
    /// # Errors
    /// Returns [`SchemaError::EmptyContent`] or the validation error of a content part.
    pub fn validate(&self) -> Result<(), SchemaError> {
        validate_content("tool_result.content", &self.content)
    }

    /// Returns the id of the call this result answers.
    #[must_use]
    pub const fn id(&self) -> &ToolCallId {
        &self.id
    }

    /// Returns the result content.
    #[must_use]
    pub fn content(&self) -> &[ContentPart] {
        &self.content
    }

    /// Reports whether the tool failed.
    #[must_use]
    pub const fn is_error(&self) -> bool {
        self.is_error
    }
}

/// How the model may choose a tool for this request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolChoice {
    /// Let the model decide whether to call a tool.
    #[default]
    Auto,
    /// Never call a tool.
    None,
    /// Must call one of the declared tools.
    Required,
    /// Must call the named tool, which the request must declare.
    Tool(ToolName),
}

fn validate_content(field: &'static str, content: &[ContentPart]) -> Result<(), SchemaError> {
    if content.is_empty() {
        return Err(SchemaError::Empty { field });
    }
    for part in content {
        part.validate()?;
    }
    Ok(())
}

fn is_false(value: &bool) -> bool {
    !*value
}
