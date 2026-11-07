//! Encoding a canonical response as a Messages response document.

use serde::Serialize;

use super::{PROTOCOL, blocks, finish_reason, usage};
use crate::{
    protocol::{ClientReply, CodecError},
    schema::{ChatResponse, Choice, Extensions},
};

/// The `message` object a complete answer carries; the same shape `message_start` opens a stream with.
pub(super) const MESSAGE_OBJECT: &str = "message";

/// Encodes one complete response as the client sees it.
///
/// The status is the protocol's own: a finished answer is a `200` whatever status the provider framed
/// its body with.
pub(super) fn encode(response: &ChatResponse) -> Result<ClientReply, CodecError> {
    let choice = single_choice(response)?;
    let wire = ResponseWire {
        id: response.id().as_str(),
        kind: MESSAGE_OBJECT,
        role: "assistant",
        model: response.model().as_str(),
        content: blocks::assistant_blocks(choice.turn())?,
        stop_reason: finish_reason(choice.finish_reason()),
        stop_sequence: None,
        usage: usage::encode(response.usage()),
        extensions: response.extensions(),
    };
    reject_shadowing_extensions(response)?;
    let body = serde_json::to_vec(&wire).map_err(|error| CodecError::Encode {
        protocol: PROTOCOL,
        reason: error.to_string(),
    })?;
    Ok(ClientReply::new(200, body))
}

/// Returns the one candidate this protocol can express.
///
/// The protocol has a single assistant message per response. A cross-protocol route cannot produce a
/// second one today — every client codec refuses `n > 1` — so a second candidate is a value this
/// codec cannot write rather than a case to approximate.
fn single_choice(response: &ChatResponse) -> Result<&Choice, CodecError> {
    match response.choices() {
        [choice] => Ok(choice),
        _ => Err(CodecError::Encode {
            protocol: PROTOCOL,
            reason: "an Anthropic message has exactly one candidate".to_owned(),
        }),
    }
}

/// Refuses an extension key that would be written next to a modelled field of this document.
///
/// Refusing is deliberate: silently skipping the entry would drop a field an adapter asked to
/// preserve, and writing it would produce a document with two members of the same name.
fn reject_shadowing_extensions(response: &ChatResponse) -> Result<(), CodecError> {
    const MODELLED: [&str; 8] = [
        "id",
        "type",
        "role",
        "model",
        "content",
        "stop_reason",
        "stop_sequence",
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
struct ResponseWire<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    role: &'static str,
    model: &'a str,
    content: Vec<blocks::BlockOut<'a>>,
    stop_reason: &'static str,
    /// Always present and always `null`: the canonical model does not record which sequence matched.
    stop_sequence: Option<&'a str>,
    usage: usage::UsageWire,
    #[serde(flatten)]
    extensions: &'a Extensions,
}
