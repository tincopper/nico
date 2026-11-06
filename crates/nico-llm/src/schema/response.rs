//! The canonical chat response: one per choice, with usage and unmodelled fields.
//!
//! A response is what a provider adapter decodes and a client codec encodes. It is *not* a
//! passthrough body: choices carry structured [`AssistantTurn`] values so a response that mixes
//! text, tool calls and reasoning survives a cross-protocol conversion.

use std::{collections::BTreeSet, fmt};

use serde::{Deserialize, Serialize};

use super::{
    ContentPart, Extensions, ModelId, Reasoning, SchemaError, ToolCall, Usage, error::excerpt,
};

/// A non-empty provider response identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct ResponseId(String);

impl ResponseId {
    /// Builds a response id.
    ///
    /// # Errors
    /// Returns [`SchemaError::Empty`] when the id is empty or only whitespace.
    pub fn try_new(value: impl Into<String>) -> Result<Self, SchemaError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(SchemaError::Empty {
                field: "response.id",
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

impl TryFrom<String> for ResponseId {
    type Error = SchemaError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl fmt::Display for ResponseId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Why the model stopped producing output.
///
/// The set is closed and semantic: each value is a reason every target client protocol can express.
/// A provider-specific reason that has no counterpart maps to the closest value here, and a reason
/// that would change client behaviour is a codec decision, not a new string in the shared model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FinishReason {
    /// The model reached a natural end.
    Stop,
    /// The output token ceiling was reached.
    Length,
    /// The model wants a tool call before continuing.
    ToolCalls,
    /// A content policy stopped generation.
    ContentFilter,
    /// The provider reported a failure after producing part of the response.
    Error,
}

/// One assistant turn: what the model said, which tools it called, and its hidden reasoning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantTurn {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    content: Vec<ContentPart>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasoning: Option<Reasoning>,
}

impl AssistantTurn {
    /// Builds a turn with neither reasoning nor completion metadata.
    ///
    /// A turn may be empty when the provider stopped before producing anything (a content filter,
    /// for example), so only the parts that are present are validated.
    ///
    /// # Errors
    /// Returns the validation error of a content part or a tool call, or
    /// [`SchemaError::Duplicate`] when two calls share an id.
    pub fn try_new(
        content: Vec<ContentPart>,
        tool_calls: Vec<ToolCall>,
    ) -> Result<Self, SchemaError> {
        let turn = Self {
            content,
            tool_calls,
            reasoning: None,
        };
        turn.validate()?;
        Ok(turn)
    }

    /// Attaches the model's hidden reasoning.
    #[must_use]
    pub fn with_reasoning(mut self, reasoning: Reasoning) -> Self {
        self.reasoning = Some(reasoning);
        self
    }

    /// Checks the content, the calls and the reasoning.
    ///
    /// # Errors
    /// Returns the validation error of a content part or a call, [`SchemaError::Duplicate`] for a
    /// repeated call id, or the reasoning's validation error.
    pub fn validate(&self) -> Result<(), SchemaError> {
        for part in &self.content {
            part.validate()?;
        }
        let mut seen = BTreeSet::new();
        for call in &self.tool_calls {
            call.validate()?;
            if !seen.insert(call.id().as_str()) {
                return Err(SchemaError::Duplicate {
                    kind: "tool call id",
                    value: excerpt(call.id().as_str()),
                });
            }
        }
        if let Some(reasoning) = &self.reasoning {
            reasoning.validate()?;
        }
        Ok(())
    }

    /// Returns the visible content.
    #[must_use]
    pub fn content(&self) -> &[ContentPart] {
        &self.content
    }

    /// Returns the tool calls the model requested.
    #[must_use]
    pub fn tool_calls(&self) -> &[ToolCall] {
        &self.tool_calls
    }

    /// Returns the model's hidden reasoning, when it produced any.
    #[must_use]
    pub const fn reasoning(&self) -> Option<&Reasoning> {
        self.reasoning.as_ref()
    }
}

/// One candidate answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    index: u32,
    turn: AssistantTurn,
    finish_reason: FinishReason,
}

impl Choice {
    /// Builds a choice.
    ///
    /// # Errors
    /// Returns the validation error of its assistant turn.
    pub fn try_new(
        index: u32,
        turn: AssistantTurn,
        finish_reason: FinishReason,
    ) -> Result<Self, SchemaError> {
        turn.validate()?;
        Ok(Self {
            index,
            turn,
            finish_reason,
        })
    }

    /// Returns the index a client uses to address this choice.
    #[must_use]
    pub const fn index(&self) -> u32 {
        self.index
    }

    /// Returns the assistant turn.
    #[must_use]
    pub const fn turn(&self) -> &AssistantTurn {
        &self.turn
    }

    /// Returns why generation stopped.
    #[must_use]
    pub const fn finish_reason(&self) -> FinishReason {
        self.finish_reason
    }
}

/// A complete non-streamed response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatResponse {
    id: ResponseId,
    model: ModelId,
    choices: Vec<Choice>,
    #[serde(default, skip_serializing_if = "Usage::is_empty")]
    usage: Usage,
    #[serde(flatten)]
    extensions: Extensions,
}

impl ChatResponse {
    /// Builds a response with no usage and no extensions.
    ///
    /// # Errors
    /// Returns [`SchemaError::EmptyChoices`] when `choices` is empty,
    /// [`SchemaError::DuplicateChoiceIndex`] when two choices share an index, or the validation
    /// error of a choice.
    pub fn try_new(
        id: ResponseId,
        model: ModelId,
        choices: Vec<Choice>,
    ) -> Result<Self, SchemaError> {
        let response = Self {
            id,
            model,
            choices,
            usage: Usage::default(),
            extensions: Extensions::new(),
        };
        response.validate()?;
        Ok(response)
    }

    /// Attaches provider-reported or gateway-measured usage.
    #[must_use]
    pub fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = usage;
        self
    }

    /// Replaces the unmodelled fields.
    ///
    /// Keys are checked against the modelled field names by [`ChatResponse::validate`], not here:
    /// whether a key shadows a field depends on the whole shape, so a response built with this
    /// method must be validated before it is used.
    #[must_use]
    pub fn with_extensions(mut self, extensions: Extensions) -> Self {
        self.extensions = extensions;
        self
    }

    /// Checks the whole response tree.
    ///
    /// Call this after deserializing a response from a fixture or diagnostic representation.
    ///
    /// # Errors
    /// Returns [`SchemaError::EmptyChoices`], [`SchemaError::DuplicateChoiceIndex`], the validation
    /// error of a choice, or [`SchemaError::ReservedExtensionKey`] when an extension key shadows a
    /// modelled field.
    pub fn validate(&self) -> Result<(), SchemaError> {
        if self.choices.is_empty() {
            return Err(SchemaError::EmptyChoices);
        }
        let mut indexes = BTreeSet::new();
        for choice in &self.choices {
            choice.turn.validate()?;
            if !indexes.insert(choice.index) {
                return Err(SchemaError::DuplicateChoiceIndex {
                    index: choice.index,
                });
            }
        }
        self.extensions
            .reject_modelled_keys(&["id", "model", "choices", "usage"])?;
        self.extensions.validate()
    }

    /// Returns the provider response id.
    #[must_use]
    pub const fn id(&self) -> &ResponseId {
        &self.id
    }

    /// Returns the physical model that produced the response.
    #[must_use]
    pub const fn model(&self) -> &ModelId {
        &self.model
    }

    /// Returns the candidate answers.
    #[must_use]
    pub fn choices(&self) -> &[Choice] {
        &self.choices
    }

    /// Returns the token usage.
    #[must_use]
    pub const fn usage(&self) -> &Usage {
        &self.usage
    }

    /// Returns the fields the canonical model does not name.
    #[must_use]
    pub const fn extensions(&self) -> &Extensions {
        &self.extensions
    }
}
