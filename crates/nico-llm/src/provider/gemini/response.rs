//! The `GenerateContentResponse` document, and the fold that turns a buffered answer into a response.
//!
//! This dialect's endpoint is always the streaming operation (see the module documentation of
//! [`super`]), so even a caller that asked for a buffered answer receives an event stream:
//! [`decode`] feeds the whole body through the same decoder a stream uses and folds its canonical
//! events into one [`ChatResponse`](crate::schema::ChatResponse). The result is that one event source
//! serves both shapes, and the two paths cannot disagree about what the answer says.
//!
//! # One chunk
//!
//! Each event's payload is one `GenerateContentResponse`: the candidates it adds, its usage counts,
//! and — on the first chunk — the response id and the model version. [`ResponseWire`] declares the
//! members this adapter uses. The *envelope* (the chunk and the candidate) is open: this dialect adds
//! metadata beside the answer (`safetyRatings`, `citationMetadata`, `groundingMetadata`, …), and a
//! member that describes ratings rather than content is consumed rather than refused — the same line
//! the Anthropic decoder draws for a top-level response field. A part is answer-bearing, so a part's
//! unknown members *are* refused; that check lives in [`super::parts`].
//!
//! # The finish reason
//!
//! | Wire | Canonical |
//! |---|---|
//! | `STOP` | `Stop` |
//! | `MAX_TOKENS` | `Length` |
//! | `SAFETY`, `RECITATION`, `BLOCKLIST`, `PROHIBITED_CONTENT`, `SPII` | `ContentFilter` |
//! | `MALFORMED_FUNCTION_CALL`, `OTHER` | `Error` |
//! | anything else | refused (`Malformed`) |
//!
//! `STOP` is also what this dialect reports after a function call, so a tool-calling answer ends with
//! `Stop` and carries its calls as content. A reason the canonical model has no value for is refused
//! rather than mapped onto the nearest one: the caller orders its next turn by that value.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::{fold, malformed, stream, usage::UsageWire};
use crate::{
    provider::{ProviderError, ProviderStream, ResponseFacts},
    schema::{ChatResponse, FinishReason, RawJson},
};

/// Decodes one provider response body into the canonical response.
///
/// The body of a success is an event stream, not a document; a failure status never reaches the
/// stream parser because the status table maps it first.
///
/// # Errors
/// Returns [`ProviderError::Upstream`] for a failure status, and [`ProviderError::Malformed`] when the
/// body is not a complete event stream, carries a part this adapter cannot read, or ends without a
/// finish reason.
pub(super) fn decode(head: &ResponseFacts, body: &[u8]) -> Result<ChatResponse, ProviderError> {
    if !head.is_success() {
        return Err(ProviderError::Upstream {
            error: super::error::failure(head.status(), body),
        });
    }
    let mut decoder = stream::Decoder::new();
    let mut events = decoder.push(body)?;
    events.extend(decoder.finish()?);
    fold::build(events)
}

/// One chunk of the event stream.
#[derive(Deserialize)]
pub(super) struct ResponseWire {
    #[serde(default)]
    pub(super) candidates: Vec<CandidateWire>,
    #[serde(default, rename = "usageMetadata")]
    pub(super) usage: Option<UsageWire>,
    #[serde(default, rename = "modelVersion")]
    pub(super) model_version: Option<String>,
    #[serde(default, rename = "responseId")]
    pub(super) response_id: Option<String>,
    #[serde(default, rename = "promptFeedback")]
    pub(super) prompt_feedback: Option<PromptFeedbackWire>,
    #[serde(default, rename = "createTime")]
    pub(super) _create_time: Option<RawJson>,
    #[serde(flatten)]
    pub(super) _extra: BTreeMap<String, RawJson>,
}

/// One candidate of a chunk.
#[derive(Deserialize)]
pub(super) struct CandidateWire {
    #[serde(default)]
    pub(super) index: Option<u32>,
    #[serde(default)]
    pub(super) content: Option<ContentIn>,
    #[serde(default, rename = "finishReason")]
    pub(super) finish_reason: Option<String>,
    // Members that describe a rating, a citation or a log probability rather than the answer's
    // content. They have no canonical place and cannot change what the caller reads, so they are
    // consumed here instead of being refused.
    #[serde(default, rename = "finishMessage")]
    pub(super) _finish_message: Option<RawJson>,
    #[serde(default, rename = "safetyRatings")]
    pub(super) _safety_ratings: Option<RawJson>,
    #[serde(default, rename = "citationMetadata")]
    pub(super) _citation_metadata: Option<RawJson>,
    #[serde(default, rename = "groundingMetadata")]
    pub(super) _grounding_metadata: Option<RawJson>,
    #[serde(default, rename = "avgLogprobs")]
    pub(super) _avg_logprobs: Option<RawJson>,
    #[serde(default, rename = "logprobsResult")]
    pub(super) _logprobs_result: Option<RawJson>,
    #[serde(default, rename = "urlContextMetadata")]
    pub(super) _url_context_metadata: Option<RawJson>,
    #[serde(default, rename = "tokenCount")]
    pub(super) _token_count: Option<RawJson>,
    #[serde(flatten)]
    pub(super) _extra: BTreeMap<String, RawJson>,
}

/// The content of one candidate.
#[derive(Deserialize)]
pub(super) struct ContentIn {
    #[serde(default)]
    pub(super) role: Option<String>,
    #[serde(default)]
    pub(super) parts: Vec<super::parts::PartIn>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

impl ContentIn {
    /// Returns the parts, refusing a member this adapter does not model.
    ///
    /// # Errors
    /// Returns [`ProviderError::Malformed`] naming the unknown member.
    pub(super) fn into_parts(self) -> Result<Vec<super::parts::PartIn>, ProviderError> {
        super::reject_extra(&self.extra, "candidates[].content")?;
        Ok(self.parts)
    }
}

/// The provider's verdict on the prompt, when it rejected it before generating.
#[derive(Deserialize)]
pub(super) struct PromptFeedbackWire {
    #[serde(default, rename = "blockReason")]
    block_reason: Option<RawJson>,
    #[serde(flatten)]
    _extra: BTreeMap<String, RawJson>,
}

impl PromptFeedbackWire {
    /// Returns the reason the prompt was blocked, when the answer reports one.
    ///
    /// A blocked prompt is a content-filter outcome, and the canonical model expresses it as a finish
    /// reason with no content — the alternative (ignoring the field) would turn a provider decision
    /// into a missing finish reason, which this adapter reports as an unreadable answer.
    pub(super) fn blocked(&self) -> bool {
        self.block_reason
            .as_ref()
            .and_then(|value| value.as_value().as_str())
            .is_some_and(|reason| !reason.is_empty())
    }
}

/// Returns the canonical finish reason for its wire spelling.
///
/// # Errors
/// Returns [`ProviderError::Malformed`] for a reason this adapter does not recognize; guessing the
/// closest value would tell the caller something the provider did not say.
pub(super) fn finish_reason(name: &str) -> Result<FinishReason, ProviderError> {
    match name {
        "STOP" => Ok(FinishReason::Stop),
        "MAX_TOKENS" => Ok(FinishReason::Length),
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" => {
            Ok(FinishReason::ContentFilter)
        }
        "MALFORMED_FUNCTION_CALL" | "OTHER" => Ok(FinishReason::Error),
        _ => Err(malformed(
            "the answer reports a finish reason this adapter does not recognize",
        )),
    }
}
