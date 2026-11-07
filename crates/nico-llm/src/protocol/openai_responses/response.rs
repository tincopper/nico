//! Encoding a canonical response as a `response` document.

use serde::Serialize;

use super::{PROTOCOL, items, usage};
use crate::{
    protocol::{ClientReply, CodecError},
    schema::{ChatResponse, Choice, Extensions, FinishReason},
};

/// The object name a complete answer carries.
const OBJECT: &str = "response";

/// Encodes one complete response as the client sees it.
pub(super) fn encode(response: &ChatResponse) -> Result<ClientReply, CodecError> {
    reject_shadowing_extensions(response)?;
    let choice = single_choice(response)?;
    let output = items::from_turn(choice.turn())?;
    let (status, details) = status(choice);
    let wire = ResponseWire {
        id: response.id().as_str(),
        object: OBJECT,
        created_at: 0,
        model: response.model().as_str(),
        status,
        incomplete_details: details,
        output,
        usage: usage::encode(response.usage()),
        extensions: response.extensions(),
    };
    let body = serde_json::to_vec(&wire).map_err(|error| CodecError::Encode {
        protocol: PROTOCOL,
        reason: error.to_string(),
    })?;
    Ok(ClientReply::new(200, body))
}

/// Returns the one candidate this protocol can express.
fn single_choice(response: &ChatResponse) -> Result<&Choice, CodecError> {
    match response.choices() {
        [choice] => Ok(choice),
        _ => Err(CodecError::Encode {
            protocol: PROTOCOL,
            reason: "a response document carries one output array".to_owned(),
        }),
    }
}

/// Returns the `status` and, when the answer was cut short, its `incomplete_details`.
fn status(choice: &Choice) -> (&'static str, Option<IncompleteDetails>) {
    match choice.finish_reason() {
        FinishReason::Stop | FinishReason::ToolCalls => ("completed", None),
        FinishReason::Length => (
            "incomplete",
            Some(IncompleteDetails {
                reason: "max_output_tokens",
            }),
        ),
        FinishReason::ContentFilter => (
            "incomplete",
            Some(IncompleteDetails {
                reason: "content_filter",
            }),
        ),
        FinishReason::Error => ("failed", None),
    }
}

/// Refuses an extension key that would be written next to a modelled member.
fn reject_shadowing_extensions(response: &ChatResponse) -> Result<(), CodecError> {
    const MODELLED: [&str; 8] = [
        "id",
        "object",
        "created_at",
        "model",
        "status",
        "incomplete_details",
        "output",
        "usage",
    ];
    for (key, _) in response.extensions().iter() {
        if MODELLED.contains(&key) {
            return Err(CodecError::Encode {
                protocol: PROTOCOL,
                reason: format!("extension key `{key}` shadows a field of this document"),
            });
        }
    }
    Ok(())
}

/// The response document.
#[derive(Serialize)]
pub(super) struct ResponseWire<'a> {
    /// The provider response id.
    pub(super) id: &'a str,
    /// Always `response`.
    pub(super) object: &'static str,
    /// The canonical model records no creation time; `0` cannot be mistaken for one.
    pub(super) created_at: u64,
    /// The physical model.
    pub(super) model: &'a str,
    /// `completed`, `incomplete` or `failed`.
    pub(super) status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) incomplete_details: Option<IncompleteDetails>,
    /// The assistant's output items.
    pub(super) output: Vec<items::OutputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) usage: Option<usage::UsageWire>,
    #[serde(flatten)]
    pub(super) extensions: &'a Extensions,
}

/// Why an answer stopped before it finished.
#[derive(Debug, Clone, Copy, Serialize)]
pub(super) struct IncompleteDetails {
    /// The protocol's own word for the cause.
    pub(super) reason: &'static str,
}

/// A response object with an empty `output`, for the event that opens a stream.
pub(super) fn opening<'a>(id: &'a str, model: &'a str) -> ResponseWire<'a> {
    ResponseWire {
        id,
        object: OBJECT,
        created_at: 0,
        model,
        status: "in_progress",
        incomplete_details: None,
        output: Vec::new(),
        usage: None,
        extensions: empty_extensions(),
    }
}

/// The extensions value a synthesized response object carries.
fn empty_extensions() -> &'static Extensions {
    static EMPTY: std::sync::OnceLock<Extensions> = std::sync::OnceLock::new();
    EMPTY.get_or_init(Extensions::new)
}
