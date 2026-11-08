//! Encoding a canonical response as one `GenerateContentResponse` document.
//!
//! One canonical answer is one candidate of this dialect, so the document carries a single entry in
//! `candidates[]`, at index zero — the only one this protocol addresses. The turn's order on the
//! wire is the model's own: the thought first, then the visible text, then the calls, which is the
//! order a model produces them in and the order the provider adapter's fold preserves.
//!
//! # What is not written
//!
//! - An image or file part in an answer is an `Encode` failure. This gateway's providers never
//!   produce one (the canonical stream has no image delta for them to arrive as), so writing
//!   `inlineData` here would describe an answer no path of this exchange can carry.
//! - A `usageMetadata` with no reported count is omitted, not written as zeros.
//! - The canonical response's stable identity is already on the document (`responseId`,
//!   `modelVersion`); an extension key beside them is refused rather than silently winning.

use serde::Serialize;

use super::{PROTOCOL, parts, usage};
use crate::{
    protocol::{ClientReply, CodecError},
    schema::{AssistantTurn, ChatResponse, Choice, ContentPart, Extensions, FinishReason},
};

/// Encodes one complete response as the client sees it.
pub(super) fn encode(response: &ChatResponse) -> Result<ClientReply, CodecError> {
    reject_shadowing_extensions(response)?;
    let choice = single_choice(response)?;
    let mut wire_parts = Vec::new();
    from_turn(choice.turn(), &mut wire_parts)?;
    let wire = ResponseWire {
        response_id: response.id().as_str(),
        model_version: response.model().as_str(),
        candidates: vec![CandidateWire {
            content: (!wire_parts.is_empty()).then_some(parts::ContentOut {
                role: parts::MODEL,
                parts: wire_parts,
            }),
            finish_reason: finish_reason(choice.finish_reason()),
        }],
        usage: usage::encode(response.usage()),
        extensions: response.extensions(),
    };
    let body = serde_json::to_vec(&wire).map_err(|failure| CodecError::Encode {
        protocol: PROTOCOL,
        reason: failure.to_string(),
    })?;
    Ok(ClientReply::new(200, body))
}

/// Returns the one candidate this protocol can express, at the position it addresses.
fn single_choice(response: &ChatResponse) -> Result<&Choice, CodecError> {
    let [choice] = response.choices() else {
        return Err(CodecError::Encode {
            protocol: PROTOCOL,
            reason: "a response document carries one candidate".to_owned(),
        });
    };
    if choice.index() != 0 {
        return Err(CodecError::Encode {
            protocol: PROTOCOL,
            reason: "the answer's candidate is not the one this protocol addresses".to_owned(),
        });
    }
    Ok(choice)
}

/// Builds the candidate's parts: the thought, then the text, then the calls.
///
/// # Errors
/// Returns [`CodecError::Encode`] for an answer part this codec cannot write (see the module doc).
pub(super) fn from_turn<'a>(
    turn: &'a AssistantTurn,
    out: &mut Vec<parts::PartWire<'a>>,
) -> Result<(), CodecError> {
    if let Some(reasoning) = turn.reasoning() {
        out.push(
            parts::PartOut::Thought {
                text: reasoning.text(),
                signature: reasoning.signature(),
            }
            .into(),
        );
    }
    for part in turn.content() {
        match part {
            ContentPart::Text(text) => out.push(parts::PartOut::Text(text).into()),
            ContentPart::Image(_) | ContentPart::File { .. } => {
                return Err(CodecError::Encode {
                    protocol: PROTOCOL,
                    reason: "an answer part of this kind has no member this codec writes"
                        .to_owned(),
                });
            }
        }
    }
    for call in turn.tool_calls() {
        out.push(
            parts::PartOut::Call {
                id: call.id().as_str(),
                name: call.name().as_str(),
                arguments: call.arguments(),
            }
            .into(),
        );
    }
    Ok(())
}

/// Returns the wire's reason for the canonical one.
///
/// The table is the provider adapter's read in reverse. `STOP` is also what this dialect reports
/// after a function call — the calls are the content, and the answer ended — so `ToolCalls` writes
/// it too; a reason this dialect spells several ways (`SAFETY` and its siblings) takes its
/// representative spelling.
pub(super) fn finish_reason(reason: FinishReason) -> &'static str {
    match reason {
        FinishReason::Stop | FinishReason::ToolCalls => "STOP",
        FinishReason::Length => "MAX_TOKENS",
        FinishReason::ContentFilter => "SAFETY",
        FinishReason::Error => "OTHER",
    }
}

/// Refuses an extension key that would be written next to a modelled member.
fn reject_shadowing_extensions(response: &ChatResponse) -> Result<(), CodecError> {
    const MODELLED: [&str; 4] = ["responseId", "modelVersion", "candidates", "usageMetadata"];
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
struct ResponseWire<'a> {
    #[serde(rename = "responseId")]
    response_id: &'a str,
    #[serde(rename = "modelVersion")]
    model_version: &'a str,
    candidates: Vec<CandidateWire<'a>>,
    #[serde(rename = "usageMetadata", skip_serializing_if = "Option::is_none")]
    usage: Option<usage::UsageWire>,
    #[serde(flatten)]
    extensions: &'a Extensions,
}

/// One candidate: this document always carries exactly one.
#[derive(Serialize)]
struct CandidateWire<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<parts::ContentOut<'a>>,
    #[serde(rename = "finishReason")]
    finish_reason: &'static str,
}
