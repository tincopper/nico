//! Decoding a Chat Completions response document into canonical values.
//!
//! A success body is read into a [`ChatResponse`]; a failure status never reaches this module's parser
//! because [`super::error`] maps it first, without decoding the provider's message. Every structural
//! field this document does not model is either preserved at the top level (`extensions`) or refused
//! with [`ProviderError::Malformed`](crate::ProviderError::Malformed):
//!
//! - `object` and `created` describe the provider's framing and are consumed, not preserved: writing
//!   `created` into the extensions would collide with the client codec's own `created` field and make
//!   it refuse an otherwise valid answer. `created` is read without inspection, so a value this
//!   adapter never uses cannot fail the document.
//! - An unknown *top-level* field is preserved and written back by the client codec.
//! - An unknown field inside the answer tree (`choices[].logprobs`, `message.refusal`,
//!   `message.audio`, an unknown member of a message) has no canonical home and is refused; a nested
//!   field silently dropped would be a fact the caller believed it asked for.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::{
    finish_reason, malformed, parse, reject_extra, schema_failure,
    usage::{self, UsageWire},
};
use crate::{
    provider::{ProviderError, ResponseFacts},
    schema::{
        AssistantTurn, ChatResponse, Choice, ContentPart, Extensions, ModelId, RawJson, Reasoning,
        ResponseId, ToolCall, ToolCallId, ToolName,
    },
};

/// The `object` value a complete answer carries.
const COMPLETION_OBJECT: &str = "chat.completion";

/// Decodes one provider response.
///
/// # Errors
/// Returns [`ProviderError::Upstream`] for a failure status, and
/// [`ProviderError::Malformed`] when a success body is not a complete chat answer this adapter can
/// express.
pub(super) fn decode(head: &ResponseFacts, body: &[u8]) -> Result<ChatResponse, ProviderError> {
    if !head.is_success() {
        return Err(ProviderError::Upstream {
            error: super::error::failure(head.status(), body),
        });
    }
    let wire: ResponseWire = parse(body, "the response body")?;
    wire.into_response()
}

/// A complete chat answer.
#[derive(Deserialize)]
struct ResponseWire {
    id: String,
    #[serde(default)]
    object: Option<String>,
    #[serde(default, rename = "created")]
    _created: Option<RawJson>,
    model: String,
    choices: Vec<ChoiceWire>,
    #[serde(default)]
    usage: Option<UsageWire>,
    #[serde(flatten)]
    extensions: Extensions,
}

impl ResponseWire {
    /// Builds the canonical response.
    fn into_response(self) -> Result<ChatResponse, ProviderError> {
        if let Some(object) = &self.object {
            if object != COMPLETION_OBJECT {
                return Err(malformed(format!(
                    "the document is not a complete chat answer (its `object` is not \
                     `{COMPLETION_OBJECT}`)"
                )));
            }
        }
        let id = ResponseId::try_new(self.id).map_err(schema_failure)?;
        let model = ModelId::try_new(self.model).map_err(schema_failure)?;
        let choices = self
            .choices
            .into_iter()
            .map(ChoiceWire::into_choice)
            .collect::<Result<Vec<_>, _>>()?;
        let mut response = ChatResponse::try_new(id, model, choices).map_err(schema_failure)?;
        if let Some(usage) = &self.usage {
            response = response.with_usage(usage::decode(usage));
        }
        response = response.with_extensions(self.extensions);
        response.validate().map_err(schema_failure)?;
        Ok(response)
    }
}

/// One candidate answer.
#[derive(Deserialize)]
struct ChoiceWire {
    index: u32,
    message: MessageWire,
    finish_reason: String,
    #[serde(default)]
    logprobs: Option<RawJson>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

impl ChoiceWire {
    /// Builds one choice.
    fn into_choice(self) -> Result<Choice, ProviderError> {
        reject_extra(&self.extra, "choices[]")?;
        if self.logprobs.is_some() {
            return Err(malformed(
                "the answer carries log-probabilities, which this adapter cannot express",
            ));
        }
        let finish = finish_reason(&self.finish_reason)?;
        let turn = self.message.into_turn()?;
        Choice::try_new(self.index, turn, finish).map_err(schema_failure)
    }
}

/// The assistant message of one candidate answer.
#[derive(Deserialize)]
struct MessageWire {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallWire>>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    refusal: Option<RawJson>,
    #[serde(default)]
    function_call: Option<RawJson>,
    #[serde(default)]
    audio: Option<RawJson>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

impl MessageWire {
    /// Builds the assistant turn.
    fn into_turn(self) -> Result<AssistantTurn, ProviderError> {
        if let Some(role) = &self.role {
            if role != "assistant" {
                return Err(malformed("the answer's message is not an assistant turn"));
            }
        }
        reject_extra(&self.extra, "choices[].message")?;
        for (name, present) in [
            ("refusal", self.refusal.is_some()),
            ("function_call", self.function_call.is_some()),
            ("audio", self.audio.is_some()),
        ] {
            if present {
                return Err(malformed(format!(
                    "the answer carries `{name}`, which this adapter cannot express"
                )));
            }
        }
        // An empty string and a null both mean the turn produced no visible text; a tool-only turn
        // is spelled that way. A text part of zero length would be a part that says nothing.
        let content = match self.content {
            Some(text) if !text.is_empty() => vec![ContentPart::text(text)],
            _ => Vec::new(),
        };
        let calls = self
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .map(ToolCallWire::into_call)
            .collect::<Result<Vec<_>, _>>()?;
        let mut turn = AssistantTurn::try_new(content, calls).map_err(schema_failure)?;
        // Providers that do not support reasoning send an empty field on the turns that carry none.
        if let Some(reasoning) = self
            .reasoning_content
            .filter(|text| !text.trim().is_empty())
        {
            turn =
                turn.with_reasoning(Reasoning::try_new(reasoning, None).map_err(schema_failure)?);
        }
        Ok(turn)
    }
}

/// One call the model asked the caller to make.
#[derive(Deserialize)]
struct ToolCallWire {
    id: String,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    function: FunctionCallWire,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

/// A call's name and arguments; the arguments arrive as a JSON string.
#[derive(Deserialize)]
struct FunctionCallWire {
    name: String,
    #[serde(default)]
    arguments: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

impl ToolCallWire {
    /// Builds one tool call.
    fn into_call(self) -> Result<ToolCall, ProviderError> {
        reject_extra(&self.extra, "choices[].message.tool_calls[]")?;
        reject_extra(
            &self.function.extra,
            "choices[].message.tool_calls[].function",
        )?;
        if let Some(kind) = &self.kind {
            if kind != "function" {
                return Err(malformed(
                    "the answer carries a tool call this adapter cannot express",
                ));
            }
        }
        let id = ToolCallId::try_new(self.id).map_err(schema_failure)?;
        let name = ToolName::try_new(self.function.name).map_err(schema_failure)?;
        // A provider that sends no arguments or an empty string means "no arguments", which the
        // canonical model spells as the empty object; anything else must parse as one.
        let arguments = match self.function.arguments {
            Some(text) if !text.trim().is_empty() => {
                super::parse::<serde_json::Value>(text.as_bytes(), "a tool call's arguments")
                    .map(RawJson::new)?
            }
            _ => RawJson::new(serde_json::Value::Object(serde_json::Map::new())),
        };
        ToolCall::try_new(id, name, arguments).map_err(schema_failure)
    }
}
