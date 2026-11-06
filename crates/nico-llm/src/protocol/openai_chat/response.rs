//! Encoding a canonical response as a Chat Completions response document.

use serde::Serialize;

use super::{CREATED, OBJECT, PROTOCOL, finish_reason, usage};
use crate::{
    protocol::{ClientReply, CodecError},
    schema::{AssistantTurn, ChatResponse, Choice, ContentPart, Extensions, ToolCall},
};

/// Encodes one complete response as the client sees it.
///
/// The status is the protocol's own: a finished chat answer is a `200` whatever status the provider
/// framed its body with.
pub(super) fn encode(response: &ChatResponse) -> Result<ClientReply, CodecError> {
    reject_shadowing_extensions(response)?;
    let wire = ResponseWire {
        id: response.id().as_str(),
        object: OBJECT,
        created: CREATED,
        model: response.model().as_str(),
        choices: response
            .choices()
            .iter()
            .map(encode_choice)
            .collect::<Result<Vec<_>, _>>()?,
        usage: usage::encode(response.usage()),
        extensions: response.extensions(),
    };
    let body = serde_json::to_vec(&wire).map_err(|error| CodecError::Encode {
        protocol: PROTOCOL,
        reason: error.to_string(),
    })?;
    Ok(ClientReply::new(200, body))
}

/// The response document.
#[derive(Serialize)]
struct ResponseWire<'a> {
    id: &'a str,
    object: &'static str,
    created: u64,
    model: &'a str,
    choices: Vec<ChoiceWire<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<usage::UsageWire>,
    #[serde(flatten)]
    extensions: &'a Extensions,
}

/// One candidate answer.
#[derive(Serialize)]
struct ChoiceWire<'a> {
    index: u32,
    message: MessageWire<'a>,
    finish_reason: &'static str,
}

/// The assistant message of one candidate answer.
#[derive(Serialize)]
struct MessageWire<'a> {
    role: &'static str,
    /// `null` when the turn is a tool call and carries no visible text, `""` for an empty turn.
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCallWire<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<&'a str>,
}

/// One call the model asked the caller to make.
#[derive(Serialize)]
struct ToolCallWire<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    function: FunctionWire<'a>,
}

/// A call's name and arguments; the protocol carries the arguments as a JSON *string*.
#[derive(Serialize)]
struct FunctionWire<'a> {
    name: &'a str,
    arguments: String,
}

/// Encodes one choice.
fn encode_choice(choice: &Choice) -> Result<ChoiceWire<'_>, CodecError> {
    let turn = choice.turn();
    let tool_calls = turn
        .tool_calls()
        .iter()
        .map(encode_tool_call)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ChoiceWire {
        index: choice.index(),
        message: MessageWire {
            role: "assistant",
            content: content(turn)?,
            reasoning_content: turn.reasoning().map(|reasoning| reasoning.text()),
            tool_calls: if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            },
        },
        finish_reason: finish_reason(choice.finish_reason()),
    })
}

/// Encodes one tool call.
fn encode_tool_call(call: &ToolCall) -> Result<ToolCallWire<'_>, CodecError> {
    Ok(ToolCallWire {
        id: call.id().as_str(),
        kind: "function",
        function: FunctionWire {
            name: call.name().as_str(),
            arguments: call.arguments().to_text(),
        },
    })
}

/// Joins the visible text of a turn, or reports a part this document cannot carry.
fn content(turn: &AssistantTurn) -> Result<Option<String>, CodecError> {
    let mut text = String::new();
    let mut has_text = false;
    for part in turn.content() {
        match part {
            ContentPart::Text(fragment) => {
                has_text = true;
                text.push_str(fragment);
            }
            ContentPart::Image(_) | ContentPart::File { .. } => {
                return Err(CodecError::Encode {
                    protocol: PROTOCOL,
                    reason: "the turn carries a content part Chat Completions cannot express"
                        .to_owned(),
                });
            }
        }
    }
    if has_text {
        return Ok(Some(text));
    }
    // A turn that only called tools has no visible content, which the protocol spells as `null`; a
    // turn that produced nothing at all is an empty string.
    if turn.tool_calls().is_empty() {
        Ok(Some(String::new()))
    } else {
        Ok(None)
    }
}

/// Refuses an extension key that would be written next to a modelled field of this document.
///
/// The canonical response already rejects the keys it models; the codec adds the two fields only this
/// document owns (`object`, `created`). Refusing is deliberate: silently skipping the entry would drop
/// a field an adapter asked to preserve.
fn reject_shadowing_extensions(response: &ChatResponse) -> Result<(), CodecError> {
    const MODELLED: [&str; 6] = ["id", "object", "created", "model", "choices", "usage"];
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
