//! One `parts[]` entry, in both directions.
//!
//! A part is this dialect's only content shape: text, an inline image, a thought, a tool call and a
//! tool result all live in the same list, distinguished by which members are set rather than by a
//! tag. The decoding side turns one part of a request into the canonical piece it carries; the
//! encoding side writes one piece of a canonical answer as a part.
//!
//! # The conversation inside a turn
//!
//! Text accumulates into a message so a run of text parts stays one canonical message, and a
//! thought, a call or a result becomes a canonical item of its own — so the wire order of a turn is
//! preserved by the item order. A thought is the model's reasoning, so one inside the caller's turn
//! is a bad document; the same walk refuses a result whose call is not in the conversation, because
//! the canonical [`ToolResult`] must name the call it answers.
//!
//! # Call ids
//!
//! `functionCall.id` and `functionResponse.id` are both optional while the canonical ids are
//! required. A call without one gets `call_` plus its position among the calls of the request — the
//! same prefix and rule the provider adapter applies to answers — and a result without one resolves
//! the id from the single same-named call of the conversation. A result that names no call, or that
//! is ambiguous between two same-named ones, is refused rather than paired by guesswork.
//!
//! # What is refused
//!
//! A part that mixes kinds, a part that carries nothing this codec reads, `inlineData` whose payload
//! is not base64, and a `functionResponse` whose object is not the `{"content": …}` shape both sides
//! of the exchange write. A `fileData` part is refused at the capability gate: it references a
//! stored file, while the canonical model carries inline bytes, so no canonical value could keep
//! what the caller asked for.
//!
//! `thoughtSignature` on a thought part is the canonical reasoning signature. On any other part it
//! is read and dropped: this dialect attaches the signature to whatever part it signs, and the
//! canonical model has one slot for it, beside the reasoning text. That is the same known gap the
//! provider adapter records for answers (§10); the *thought's own* signature still round-trips.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{CALL_ID_PREFIX, field, invalid, reject_extra, schema, unsupported_capability};
use crate::{
    protocol::{Capability, CodecError},
    schema::{
        ContentPart, ImageSource, Item, Message, RawJson, Reasoning, Role, ToolCall, ToolCallId,
        ToolName, ToolResult,
    },
    wire::base64,
};

/// The role this dialect gives the model's turns: the only role an answer carries.
pub(super) const MODEL: &str = "model";

/// One canonical piece a request part carries.
pub(super) enum Piece {
    /// Visible text.
    Text(String),
    /// An inline image, decoded from base64.
    Image {
        /// The image's media type.
        media_type: String,
        /// The image bytes.
        data: Vec<u8>,
    },
    /// Hidden reasoning, with its signature when the part carries one.
    Thought {
        /// The reasoning text.
        text: String,
        /// The provider's signature, when the part carries one.
        signature: Option<String>,
    },
    /// A call the model asked for; `id` is absent when the caller wrote none.
    Call {
        /// The caller's id, when one was written.
        id: Option<String>,
        /// The tool being called.
        name: String,
        /// The arguments document.
        arguments: RawJson,
    },
    /// The answer to a call, as the text its `{"content": …}` object carries.
    Response {
        /// The caller's id, when one was written.
        id: Option<String>,
        /// The tool that was called.
        name: String,
        /// The joined text of the answer.
        text: String,
    },
}

/// The calls decoded so far, in conversation order, for resolving a result's pair.
type Calls = Vec<(String, ToolCallId)>;

/// Decodes one turn's parts into canonical items.
///
/// # Errors
/// Returns a bad-document error for an unknown member, a part that mixes kinds or carries nothing
/// readable, a thought inside the caller's turn, and a result whose call cannot be resolved.
pub(super) fn turn(
    content: &super::request::ContentWire,
    index: usize,
    calls: &mut Calls,
    out: &mut Vec<Item>,
) -> Result<(), CodecError> {
    let role = content.role(index)?;
    let mut message: Vec<ContentPart> = Vec::new();
    for (position, raw) in content.parts.iter().enumerate() {
        let path = format!("contents[{index}].parts[{position}]");
        match decode_part(raw, &path)? {
            Piece::Text(text) => message.push(ContentPart::text(text)),
            Piece::Image { media_type, data } => {
                message.push(ContentPart::Image(ImageSource::Base64 { media_type, data }));
            }
            Piece::Thought { text, signature } => {
                if role == Role::User {
                    return Err(invalid(format!(
                        "field `{path}` carries a thought, which only a model turn carries"
                    )));
                }
                flush(&role, &mut message, out)?;
                let reasoning =
                    Reasoning::try_new(text, signature).map_err(|error| field(&path, error))?;
                out.push(Item::Reasoning(reasoning));
            }
            Piece::Call {
                id,
                name,
                arguments,
            } => {
                flush(&role, &mut message, out)?;
                let id = match id {
                    Some(id) => ToolCallId::try_new(id).map_err(|error| field(&path, error))?,
                    None => ToolCallId::try_new(format!("{CALL_ID_PREFIX}{}", calls.len()))
                        .map_err(|error| field(&path, error))?,
                };
                let name = ToolName::try_new(name).map_err(|error| field(&path, error))?;
                let call = ToolCall::try_new(id.clone(), name.clone(), arguments)
                    .map_err(|error| field(&path, error))?;
                calls.push((name.as_str().to_owned(), id));
                out.push(Item::ToolCall(call));
            }
            Piece::Response { id, name, text } => {
                flush(&role, &mut message, out)?;
                let id = resolve_result_id(id, &name, calls, &path)?;
                let result = ToolResult::try_new(id, vec![ContentPart::text(text)])
                    .map_err(|error| field(&path, error))?;
                out.push(Item::ToolResult(result));
            }
        }
    }
    flush(&role, &mut message, out)?;
    Ok(())
}

/// Writes the message a run of text and image parts accumulated, if any.
fn flush(
    role: &Role,
    message: &mut Vec<ContentPart>,
    out: &mut Vec<Item>,
) -> Result<(), CodecError> {
    if message.is_empty() {
        return Ok(());
    }
    let message = Message::try_new(*role, std::mem::take(message)).map_err(schema)?;
    out.push(Item::Message(message));
    Ok(())
}

/// Resolves the call a tool result answers.
///
/// An explicit id is taken as written. A result without one pairs with the single same-named call
/// of the conversation; none, or more than one, is a document this codec refuses rather than pairs
/// by guesswork.
fn resolve_result_id(
    id: Option<String>,
    name: &str,
    calls: &Calls,
    path: &str,
) -> Result<ToolCallId, CodecError> {
    if let Some(id) = id {
        return ToolCallId::try_new(id).map_err(|error| field(path, error));
    }
    let matches: Vec<&ToolCallId> = calls
        .iter()
        .filter(|(call, _)| call == name)
        .map(|(_, id)| id)
        .collect();
    match matches.as_slice() {
        [id] => Ok((*id).clone()),
        [] => Err(invalid(format!(
            "field `{path}.functionResponse` answers a call that is not in this conversation"
        ))),
        _ => Err(invalid(format!(
            "field `{path}.functionResponse` is ambiguous: more than one call in this \
             conversation carries its name"
        ))),
    }
}

/// Decodes one part of a request.
///
/// # Errors
/// Returns a bad-document error for a part that mixes kinds, carries nothing readable, or whose
/// payload is not the shape this dialect sends, and a refused capability for `fileData`.
pub(super) fn decode_part(raw: &RawJson, path: &str) -> Result<Piece, CodecError> {
    let part: PartIn = serde_json::from_value(raw.as_value().clone())
        .map_err(|error| invalid(format!("field `{path}` is invalid: {error}")))?;
    reject_extra(&part.extra, path)?;
    if part.file_data.is_some() {
        return Err(unsupported_capability(Capability::Files));
    }
    mixed(&part, path)?;
    if let Some(call) = part.function_call {
        return call.into_piece(path);
    }
    if let Some(response) = part.function_response {
        return response.into_piece(path);
    }
    if let Some(inline) = part.inline_data {
        let data = base64::decode(&inline.data).map_err(|error| {
            invalid(format!(
                "field `{path}.inlineData.data` is not decodable: {error}"
            ))
        })?;
        return Ok(Piece::Image {
            media_type: inline.mime_type,
            data,
        });
    }
    let Some(text) = part.text else {
        return Err(invalid(format!(
            "field `{path}` carries nothing this codec reads"
        )));
    };
    if part.thought.unwrap_or(false) {
        return Ok(Piece::Thought {
            text,
            signature: part.thought_signature,
        });
    }
    // A signature beside a text part signs content the canonical model has one slot for — beside
    // the reasoning — so it is dropped here; see the module documentation.
    Ok(Piece::Text(text))
}

/// Refuses a part that sets members of more than one kind.
///
/// A thought is a text part with the flag set, so `thought` alone is one kind; beside a call, a
/// result or an image it would describe content of two kinds at once.
fn mixed(part: &PartIn, path: &str) -> Result<(), CodecError> {
    let thought = part.thought.unwrap_or(false);
    let kinds = part.text.is_some() as u8
        + part.inline_data.is_some() as u8
        + part.function_call.is_some() as u8
        + part.function_response.is_some() as u8;
    let overloaded = (kinds > 1) || (thought && kinds > 1) || (thought && part.text.is_none());
    if overloaded {
        return Err(invalid(format!(
            "field `{path}` carries more than one kind"
        )));
    }
    Ok(())
}

impl CallIn {
    /// Turns one call into its canonical piece.
    fn into_piece(self, path: &str) -> Result<Piece, CodecError> {
        reject_extra(&self.extra, &format!("{path}.functionCall"))?;
        let arguments = match self.args {
            Some(args) if !args.is_object() => {
                return Err(invalid(format!(
                    "field `{path}.functionCall.args` is not a JSON object"
                )));
            }
            Some(args) => args,
            // The protocol documents `args` as the call's parameters and sends `{}` when it has
            // none; an absent member therefore means the same thing, not "unknown arguments".
            None => RawJson::new(serde_json::json!({})),
        };
        Ok(Piece::Call {
            id: self.id,
            name: self.name,
            arguments,
        })
    }
}

impl ResponseIn {
    /// Turns one result into its canonical piece.
    fn into_piece(self, path: &str) -> Result<Piece, CodecError> {
        reject_extra(&self.extra, &format!("{path}.functionResponse"))?;
        let wire = self.response.ok_or_else(|| {
            invalid(format!(
                "field `{path}.functionResponse.response` is required"
            ))
        })?;
        let serde_json::Value::Object(map) = wire.as_value() else {
            return Err(invalid(format!(
                "field `{path}.functionResponse.response` is not a JSON object"
            )));
        };
        for key in map.keys() {
            if key != "content" {
                let field = super::named(&format!("{path}.functionResponse.response"), key);
                return Err(invalid(format!(
                    "field `{field}` is not part of this document"
                )));
            }
        }
        let Some(content) = map.get("content") else {
            return Err(invalid(format!(
                "field `{path}.functionResponse.response.content` is required"
            )));
        };
        let Some(text) = content.as_str() else {
            return Err(invalid(format!(
                "field `{path}.functionResponse.response.content` is not a string"
            )));
        };
        Ok(Piece::Response {
            id: self.id,
            name: self.name,
            text: text.to_owned(),
        })
    }
}

/// One part of a request.
///
/// Every member is optional and the reader refuses a part that sets more than one kind, because the
/// protocol distinguishes parts by which field is present rather than by a tag.
#[derive(Deserialize)]
struct PartIn {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thought: Option<bool>,
    #[serde(default, rename = "thoughtSignature")]
    thought_signature: Option<String>,
    #[serde(default, rename = "inlineData")]
    inline_data: Option<InlineData>,
    #[serde(default, rename = "functionCall")]
    function_call: Option<CallIn>,
    #[serde(default, rename = "functionResponse")]
    function_response: Option<ResponseIn>,
    #[serde(default, rename = "fileData")]
    file_data: Option<RawJson>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

/// The inline bytes of an image.
#[derive(Deserialize)]
struct InlineData {
    #[serde(default, rename = "mimeType")]
    mime_type: String,
    #[serde(default)]
    data: String,
}

/// A tool call the model asked for.
#[derive(Deserialize)]
struct CallIn {
    #[serde(default)]
    id: Option<String>,
    name: String,
    #[serde(default)]
    args: Option<RawJson>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

/// The answer to a tool call.
#[derive(Deserialize)]
struct ResponseIn {
    #[serde(default)]
    id: Option<String>,
    name: String,
    #[serde(default)]
    response: Option<RawJson>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

/// One part of a canonical answer, as this protocol writes it.
pub(super) enum PartOut<'a> {
    /// Visible text.
    Text(&'a str),
    /// Hidden reasoning, with its signature when the canonical item carries one.
    Thought {
        /// The reasoning text.
        text: &'a str,
        /// The provider's signature, when there is one.
        signature: Option<&'a str>,
    },
    /// A call the model asked for.
    Call {
        /// The call id, which the next request writes back.
        id: &'a str,
        /// The tool being called.
        name: &'a str,
        /// The arguments document.
        arguments: &'a RawJson,
    },
}

/// One part of an answer document.
#[derive(Serialize, Default)]
pub(super) struct PartWire<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thought: Option<bool>,
    #[serde(rename = "thoughtSignature", skip_serializing_if = "Option::is_none")]
    thought_signature: Option<&'a str>,
    #[serde(rename = "functionCall", skip_serializing_if = "Option::is_none")]
    function_call: Option<FunctionCallOut<'a>>,
}

/// One turn of an answer: the model's role and its parts.
#[derive(Serialize)]
pub(super) struct ContentOut<'a> {
    /// Always the model's role: an answer is what the model produced.
    pub(super) role: &'static str,
    pub(super) parts: Vec<PartWire<'a>>,
}

/// A tool call an answer carries.
#[derive(Serialize)]
struct FunctionCallOut<'a> {
    id: &'a str,
    name: &'a str,
    args: &'a RawJson,
}

impl<'a> From<PartOut<'a>> for PartWire<'a> {
    fn from(part: PartOut<'a>) -> Self {
        match part {
            PartOut::Text(text) => Self {
                text: Some(text),
                ..Self::default()
            },
            PartOut::Thought { text, signature } => Self {
                text: Some(text),
                thought: Some(true),
                thought_signature: signature,
                ..Self::default()
            },
            PartOut::Call {
                id,
                name,
                arguments,
            } => Self {
                function_call: Some(FunctionCallOut {
                    id,
                    name,
                    args: arguments,
                }),
                ..Self::default()
            },
        }
    }
}
