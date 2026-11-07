//! Decoding a Responses body into canonical values.
//!
//! A success body is read into a [`ChatResponse`]; a failure status never reaches this module's parser
//! because the shared status table maps it first, without decoding the provider's message. The
//! document's members fall into three groups:
//!
//! - **The answer**: `output[]`, `usage`, `status`/`incomplete_details`, and the `id`/`model` that
//!   name it.
//! - **Envelope**: `object`, `created_at`, `error`. They are read without inspection (a value this
//!   adapter never uses must not be able to fail the document) and are not preserved: writing an
//!   envelope member into a *client's* answer would report framing the client did not ask for.
//! - **The request echo**: this dialect's response object repeats request members (`instructions`,
//!   `tools`, `text`, `temperature`, …) beside the answer. They describe the request, not the answer,
//!   so [`ENVELOPE`] consumes them; only a member this adapter does not know is preserved in
//!   [`Extensions`] and written back by a client codec. The exact echo set is the one unverified item
//!   of `research/responses-wire.md`, and the list is a superset of what this adapter reads.
//!
//! # What the answer may contain
//!
//! | `output[]` item | Canonical |
//! |---|---|
//! | `message` with `output_text` parts | the turn's visible content |
//! | `function_call` | one [`ToolCall`] on the turn |
//! | `reasoning` with `summary_text` parts | the turn's [`Reasoning`], signature empty |
//! | anything else | [`ProviderError::Malformed`] |
//!
//! An unknown item type is refused, not skipped: it can carry content the caller would otherwise see.
//! A `refusal` content part is refused for the same reason the chat adapter refuses `message.refusal`
//! — the canonical answer has no shape for it — and a citation list is refused because a citation is
//! produced by a built-in tool this adapter does not request.
//!
//! A reasoning item's `id` and `encrypted_content` are its replay handles. The canonical model has one
//! opaque field for reasoning, so neither can be carried and the item cannot be sent back; the
//! *summary text* is still rendered, because refusing to read part of the answer would hide it. That
//! is why the adapter declares no `reasoning` capability while it still reports one.
//!
//! The status decides why generation stopped; [`super::finish_reason`] holds the closed table, and a
//! 2xx document that is not a finished answer is refused rather than mapped to `stop`.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::{
    finish_reason, malformed, parse, reject_extra, schema_failure,
    usage::{self, UsageWire},
};
use crate::{
    provider::{ProviderError, ResponseFacts},
    schema::{
        AssistantTurn, ChatResponse, Choice, ContentPart, Extensions, FinishReason, ModelId,
        RawJson, Reasoning, ResponseId, ToolCall, ToolCallId, ToolName, Usage,
    },
};

/// The `object` value a complete answer carries.
const OBJECT: &str = "response";

/// The response members that repeat the request rather than describe the answer.
///
/// This dialect's response object echoes the request beside the answer. An echo is not conversation
/// content: the canonical [`Extensions`] is written back into a *client's* document, where an echoed
/// `tools` or `text` would collide with the client codec's own field of the same name, and where a
/// request the caller made would be reported back to it as if the model had said it. They are
/// therefore consumed. A member outside this list is unknown to the adapter, so it is preserved
/// rather than guessed at.
const ENVELOPE: [&str; 25] = [
    "background",
    "completed_at",
    "conversation",
    "include",
    "instructions",
    "max_output_tokens",
    "max_tool_calls",
    "metadata",
    "parallel_tool_calls",
    "previous_response_id",
    "prompt",
    "prompt_cache_key",
    "reasoning",
    "safety_identifier",
    "service_tier",
    "status_details",
    "store",
    "temperature",
    "text",
    "tool_choice",
    "tools",
    "top_logprobs",
    "top_p",
    "truncation",
    "user",
];

/// Decodes one provider response.
///
/// # Errors
/// Returns [`ProviderError::Upstream`] for a failure status, and [`ProviderError::Malformed`] when a
/// success body is not a finished answer this adapter can express.
pub(super) fn decode(head: &ResponseFacts, body: &[u8]) -> Result<ChatResponse, ProviderError> {
    if !head.is_success() {
        return Err(ProviderError::Upstream {
            error: super::super::openai::error::failure(head.status(), body),
        });
    }
    let wire: ResponseWire = parse(body, "the response body")?;
    wire.into_response()
}

/// One response-shaped document: the body of a complete answer, or the `response` member an event
/// carries.
///
/// Every member is optional here because the same value reads both shapes: a `response.created`
/// event's response has no output yet, and a complete answer has all of it. The caller that needs a
/// member requires it, and names it when it is absent.
#[derive(Deserialize)]
pub(super) struct ResponseWire {
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "object")]
    _object: Option<RawJson>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    incomplete_details: Option<IncompleteWire>,
    #[serde(default)]
    output: Option<Vec<OutputItemWire>>,
    #[serde(default)]
    usage: Option<UsageWire>,
    #[serde(default)]
    error: Option<ErrorWire>,
    #[serde(default, rename = "created_at")]
    _created_at: Option<RawJson>,
    #[serde(flatten)]
    echo: BTreeMap<String, RawJson>,
}

impl ResponseWire {
    /// Returns the response id and the physical model that produced it.
    pub(super) fn identity(&self) -> Result<(ResponseId, ModelId), ProviderError> {
        let id = self
            .id
            .as_deref()
            .ok_or_else(|| malformed("the document carries no response id"))?;
        let model = self
            .model
            .as_deref()
            .ok_or_else(|| malformed("the document names no model"))?;
        Ok((
            ResponseId::try_new(id).map_err(schema_failure)?,
            ModelId::try_new(model).map_err(schema_failure)?,
        ))
    }

    /// Returns why this answer stopped, given whether it produced any tool call.
    pub(super) fn finish(&self, has_calls: bool) -> Result<FinishReason, ProviderError> {
        let status = self
            .status
            .as_deref()
            .ok_or_else(|| malformed("the answer carries no status"))?;
        let reason = self
            .incomplete_details
            .as_ref()
            .and_then(|details| details.reason.as_deref());
        finish_reason(status, reason, has_calls)
    }

    /// Returns the usage this answer reported.
    pub(super) fn stream_usage(&self) -> Option<Usage> {
        self.usage.as_ref().map(usage::decode)
    }

    /// Returns the provider's own failure code, for the log only.
    ///
    /// The code is a bounded token when it reaches the error; the message beside it is the provider's
    /// diagnostic and is never read.
    pub(super) fn error_code(&self) -> Option<&RawJson> {
        self.error.as_ref().and_then(|error| error.code.as_ref())
    }

    /// Builds the canonical response.
    fn into_response(mut self) -> Result<ChatResponse, ProviderError> {
        if let Some(object) = &self._object
            && object.as_value().as_str() != Some(OBJECT)
        {
            return Err(malformed(
                "the document is not a complete response (its `object` is not a response)",
            ));
        }
        let (id, model) = self.identity()?;
        let output = self
            .output
            .take()
            .ok_or_else(|| malformed("the answer carries no output"))?;
        let mut turn = TurnBuilder::default();
        for item in output {
            if let Some(folded) = item.fold()? {
                turn.push(folded)?;
            }
        }
        let turn = turn.build()?;
        let finish = self.finish(!turn.tool_calls().is_empty())?;
        let choice = Choice::try_new(0, turn, finish).map_err(schema_failure)?;
        let mut response =
            ChatResponse::try_new(id, model, vec![choice]).map_err(schema_failure)?;
        if let Some(usage) = self.stream_usage() {
            response = response.with_usage(usage);
        }
        response = response.with_extensions(preserved(std::mem::take(&mut self.echo))?);
        response.validate().map_err(schema_failure)?;
        Ok(response)
    }
}

/// Keeps the members this adapter does not recognise, dropping the request echo.
fn preserved(echo: BTreeMap<String, RawJson>) -> Result<Extensions, ProviderError> {
    let mut extensions = Extensions::new();
    for (key, value) in echo {
        if ENVELOPE.contains(&key.as_str()) {
            continue;
        }
        extensions.try_insert(key, value).map_err(schema_failure)?;
    }
    Ok(extensions)
}

/// Why an answer stopped early.
#[derive(Deserialize)]
struct IncompleteWire {
    #[serde(default)]
    reason: Option<String>,
}

/// The provider's failure object inside a failed answer; only its code is read.
#[derive(Deserialize)]
struct ErrorWire {
    #[serde(default)]
    code: Option<RawJson>,
}

/// One assistant turn under construction.
#[derive(Default)]
struct TurnBuilder {
    content: Vec<ContentPart>,
    calls: Vec<ToolCall>,
    reasoning: Option<Reasoning>,
}

impl TurnBuilder {
    /// Folds one decoded item into the turn.
    fn push(&mut self, folded: Folded) -> Result<(), ProviderError> {
        match folded {
            Folded::Text(text) => self.content.push(ContentPart::text(text)),
            Folded::Call(call) => self.calls.push(call),
            Folded::Reasoning(text) => {
                // The canonical turn carries one reasoning value and the dialect sends one reasoning
                // item per answer; a second would be silently merged into the first, which is
                // exactly the hidden change this adapter refuses elsewhere.
                if self.reasoning.is_some() {
                    return Err(malformed("the answer carries more than one reasoning item"));
                }
                self.reasoning = Some(Reasoning::try_new(text, None).map_err(schema_failure)?);
            }
        }
        Ok(())
    }

    /// Builds the turn.
    fn build(self) -> Result<AssistantTurn, ProviderError> {
        let turn = AssistantTurn::try_new(self.content, self.calls).map_err(schema_failure)?;
        Ok(match self.reasoning {
            Some(reasoning) => turn.with_reasoning(reasoning),
            None => turn,
        })
    }
}

/// One decoded output item.
enum Folded {
    Text(String),
    Call(ToolCall),
    Reasoning(String),
}

/// One `output[]` item.
///
/// The item kinds share one struct instead of an enum because the dialect tags them with a string
/// member rather than by shape: an untagged enum would let a `message` with a `call_id` decode as a
/// tool call. Every member is optional here and the `type` decides which are required.
#[derive(Deserialize)]
pub(super) struct OutputItemWire {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, rename = "id")]
    _id: Option<RawJson>,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
    #[serde(default)]
    content: Option<Vec<OutputPartWire>>,
    #[serde(default)]
    summary: Option<Vec<SummaryWire>>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default, rename = "status")]
    _status: Option<RawJson>,
    #[serde(default, rename = "encrypted_content")]
    _encrypted_content: Option<RawJson>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

impl OutputItemWire {
    /// Returns the kind this item is tagged with.
    pub(super) fn kind(&self) -> &str {
        &self.kind
    }

    /// Returns the call id and tool name of a `function_call` item.
    ///
    /// A stream announces the call here and then sends its arguments as separate events, so this is
    /// the only place the canonical `ToolCallStart` can learn the name.
    pub(super) fn function_call(&self) -> Option<(&str, &str)> {
        match (self.call_id.as_deref(), self.name.as_deref()) {
            (Some(id), Some(name)) => Some((id, name)),
            _ => None,
        }
    }

    /// Folds this item into the turn, or reports that it contributes nothing.
    fn fold(self) -> Result<Option<Folded>, ProviderError> {
        reject_extra(&self.extra, "output[]")?;
        match self.kind.as_str() {
            "message" => self.fold_message(),
            "function_call" => self.fold_call().map(Some),
            "reasoning" => self.fold_reasoning(),
            // An unknown item can carry content the caller would otherwise see.
            _ => Err(malformed(
                "the answer carries an output item this adapter cannot express",
            )),
        }
    }

    /// Folds one visible message.
    fn fold_message(self) -> Result<Option<Folded>, ProviderError> {
        if let Some(role) = &self.role
            && role != "assistant"
        {
            return Err(malformed("the answer's message is not an assistant turn"));
        }
        let content = self
            .content
            .ok_or_else(|| malformed("an answer message carries no content"))?;
        let mut text = String::new();
        for part in content {
            reject_extra(&part.extra, "output[].content")?;
            match part.kind.as_str() {
                "output_text" => {
                    if let Some(annotations) = &part.annotations
                        && !annotations.is_empty()
                    {
                        return Err(malformed(
                            "the answer carries citations, which this adapter cannot express",
                        ));
                    }
                    let fragment = part
                        .text
                        .ok_or_else(|| malformed("an output text part carries no text"))?;
                    text.push_str(&fragment);
                }
                "refusal" => {
                    return Err(malformed(
                        "the answer carries a refusal, which this adapter cannot express",
                    ));
                }
                _ => {
                    return Err(malformed(
                        "the answer carries a content part this adapter cannot express",
                    ));
                }
            }
        }
        // A message whose parts are all empty says nothing; a zero-length canonical part would say
        // the same thing in a shape no client protocol asked for.
        Ok((!text.is_empty()).then_some(Folded::Text(text)))
    }

    /// Folds one tool call.
    fn fold_call(self) -> Result<Folded, ProviderError> {
        let call_id = self
            .call_id
            .ok_or_else(|| malformed("a tool call carries no call id"))?;
        let name = self
            .name
            .ok_or_else(|| malformed("a tool call carries no name"))?;
        // A call with no arguments, or with an empty string, means "no arguments", which the
        // canonical model spells as the empty object; anything else must parse as one.
        let arguments = match self.arguments {
            Some(text) if !text.trim().is_empty() => {
                parse::<serde_json::Value>(text.as_bytes(), "a tool call's arguments")
                    .map(RawJson::new)?
            }
            _ => RawJson::new(serde_json::Value::Object(serde_json::Map::new())),
        };
        Ok(Folded::Call(
            ToolCall::try_new(
                ToolCallId::try_new(call_id).map_err(schema_failure)?,
                ToolName::try_new(name).map_err(schema_failure)?,
                arguments,
            )
            .map_err(schema_failure)?,
        ))
    }

    /// Folds one reasoning item's summary into the turn's reasoning text.
    ///
    /// The `id` and `encrypted_content` members are the replay handles this adapter cannot carry; the
    /// summary is the part the caller can read, so it is rendered rather than dropped.
    fn fold_reasoning(self) -> Result<Option<Folded>, ProviderError> {
        let mut text = String::new();
        for part in self.summary.unwrap_or_default() {
            reject_extra(&part.extra, "output[].summary")?;
            if part.kind != "summary_text" {
                return Err(malformed(
                    "a reasoning summary carries a part this adapter cannot express",
                ));
            }
            if let Some(fragment) = part.text {
                text.push_str(&fragment);
            }
        }
        Ok((!text.trim().is_empty()).then_some(Folded::Reasoning(text)))
    }
}

/// One part of a visible answer message.
#[derive(Deserialize)]
struct OutputPartWire {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    annotations: Option<Vec<RawJson>>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

/// One part of a reasoning summary.
#[derive(Deserialize)]
struct SummaryWire {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}
