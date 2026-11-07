//! One `parts[]` entry, in both directions.
//!
//! A part is this dialect's only content shape: text, an inline image, a tool call, a tool result and
//! a thought all live in the same list, distinguished by which members are set. The encoding side is
//! the small set of constructors the request document uses; the decoding side turns one part of a
//! provider answer into the canonical increment it carries.
//!
//! # Refusals
//!
//! Nothing is dropped. A canonical value with no member here is a typed refusal: a remote image
//! (`fileData.fileUri` references a stored file, not an `http` URL), a file part, a tool result that
//! carries anything but text, and the same for a failed result. A provider answer whose part this
//! adapter cannot express is [`ProviderError::Malformed`]: `inlineData` (an image has no canonical
//! delta), `functionResponse` (a request shape), `fileData` (a stored-file reference), a part with
//! more than one kind, and a part with nothing this adapter reads at all.
//!
//! The *envelope* objects (a chunk, a candidate) are open — this dialect adds metadata beside the
//! answer — but a part is answer-bearing, so an unknown member inside one is refused rather than
//! ignored.
//!
//! `thoughtSignature` is read and dropped on the decoding side. This dialect attaches it to the part
//! whose content it signs (a thought, and often the call or text that followed one), and the canonical
//! event set has one reasoning increment carrying text and no signature. A complete answer is folded
//! from those events ([`super::fold`]), so the signature cannot survive the fold; the *encoding* side
//! still writes one whenever the caller replays a [`Reasoning`](crate::schema::Reasoning) item.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use super::{malformed, reject_extra, unsupported, unsupported_field};
use crate::{
    protocol::Capability,
    provider::ProviderError,
    schema::{ContentPart, ImageSource, RawJson, ToolCall},
    wire::base64,
};

/// One part of a request.
///
/// Every member is optional and the writer sets exactly one kind, because the protocol distinguishes
/// parts by which field is present rather than by a tag: an untagged enum would let a `text` part with
/// a call member decode as two things at once.
#[derive(Serialize, Default)]
pub(super) struct PartWire<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<Cow<'a, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thought: Option<bool>,
    #[serde(rename = "thoughtSignature", skip_serializing_if = "Option::is_none")]
    thought_signature: Option<&'a str>,
    #[serde(rename = "inlineData", skip_serializing_if = "Option::is_none")]
    inline_data: Option<InlineData<'a>>,
    #[serde(rename = "functionCall", skip_serializing_if = "Option::is_none")]
    function_call: Option<FunctionCall<'a>>,
    #[serde(rename = "functionResponse", skip_serializing_if = "Option::is_none")]
    function_response: Option<FunctionResponse<'a>>,
}

impl<'a> PartWire<'a> {
    /// Writes visible text.
    pub(super) fn text(text: impl Into<Cow<'a, str>>) -> Self {
        Self {
            text: Some(text.into()),
            ..Self::default()
        }
    }

    /// Writes reasoning, with the provider signature when the canonical item carries one.
    pub(super) fn thought(text: impl Into<Cow<'a, str>>, signature: Option<&'a str>) -> Self {
        Self {
            text: Some(text.into()),
            thought: Some(true),
            thought_signature: signature,
            ..Self::default()
        }
    }

    /// Writes an inline image.
    pub(super) fn inline(media_type: &'a str, data: &[u8]) -> Self {
        Self {
            inline_data: Some(InlineData {
                mime_type: media_type,
                data: base64::encode(data),
            }),
            ..Self::default()
        }
    }

    /// Writes a tool call the model asked for.
    pub(super) fn call(call: &'a ToolCall) -> Self {
        Self {
            function_call: Some(FunctionCall {
                id: call.id().as_str(),
                name: call.name().as_str(),
                args: call.arguments(),
            }),
            ..Self::default()
        }
    }

    /// Writes the answer to a tool call.
    ///
    /// `content` is the joined text of the canonical result: this dialect's `response` is a JSON
    /// object, and `{"content": …}` is the one spelling both a caller and this adapter can read back.
    pub(super) fn response(id: &'a str, name: &'a str, content: impl Into<Cow<'a, str>>) -> Self {
        Self {
            function_response: Some(FunctionResponse {
                id,
                name,
                response: StructBody {
                    content: content.into(),
                },
            }),
            ..Self::default()
        }
    }
}

/// The inline bytes of an image.
#[derive(Serialize)]
struct InlineData<'a> {
    #[serde(rename = "mimeType")]
    mime_type: &'a str,
    data: String,
}

/// A tool call the model asked for.
#[derive(Serialize)]
struct FunctionCall<'a> {
    id: &'a str,
    name: &'a str,
    args: &'a RawJson,
}

/// The answer to a tool call.
#[derive(Serialize)]
struct FunctionResponse<'a> {
    id: &'a str,
    name: &'a str,
    response: StructBody<'a>,
}

/// The object this adapter writes as a tool result's payload.
#[derive(Serialize)]
struct StructBody<'a> {
    content: Cow<'a, str>,
}

/// Appends one message's content parts.
///
/// # Errors
/// Returns [`ProviderError::UnsupportedField`] for a remote image, which has no member here, and
/// [`ProviderError::Unsupported`] for a file part.
pub(super) fn from_content<'a>(
    parts: &'a [ContentPart],
    out: &mut Vec<PartWire<'a>>,
) -> Result<(), ProviderError> {
    for part in parts {
        out.push(match part {
            ContentPart::Text(text) => PartWire::text(text),
            ContentPart::Image(ImageSource::Base64 { media_type, data }) => {
                PartWire::inline(media_type, data)
            }
            ContentPart::Image(ImageSource::Url(_)) => {
                return Err(unsupported_field("content_part.image.url"));
            }
            ContentPart::File { .. } => return Err(unsupported(Capability::Files)),
        });
    }
    Ok(())
}

/// One part of a provider answer.
///
/// Every member is optional and [`PartIn::into_part`] decides which kind the set members describe.
/// The differences from [`PartWire`] are deliberate: this side also declares the members it must
/// *refuse* (`inlineData`, `functionResponse`, `fileData`), so a refusal can name what it found
/// instead of calling a known shape unknown.
#[derive(Deserialize)]
pub(super) struct PartIn {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thought: Option<bool>,
    #[serde(default, rename = "thoughtSignature")]
    thought_signature: Option<String>,
    #[serde(default, rename = "inlineData")]
    inline_data: Option<RawJson>,
    #[serde(default, rename = "functionCall")]
    function_call: Option<FunctionCallIn>,
    #[serde(default, rename = "functionResponse")]
    function_response: Option<RawJson>,
    #[serde(default, rename = "fileData")]
    file_data: Option<RawJson>,
    #[serde(flatten)]
    extra: std::collections::BTreeMap<String, RawJson>,
}

/// The tool call a provider answer carries.
#[derive(Deserialize)]
pub(super) struct FunctionCallIn {
    #[serde(default)]
    id: Option<String>,
    name: String,
    #[serde(default)]
    args: Option<RawJson>,
    #[serde(flatten)]
    extra: std::collections::BTreeMap<String, RawJson>,
}

/// What one part of an answer carries.
pub(super) enum DecodedPart {
    /// Visible text.
    Text(String),
    /// Hidden reasoning.
    Thought(String),
    /// A call the model asked for; `id` is absent when the answer named none.
    Call {
        /// The provider's call id, when it sent one.
        id: Option<String>,
        /// The tool being called.
        name: String,
        /// The arguments document.
        arguments: RawJson,
    },
}

impl PartIn {
    /// Turns one part into the canonical increment it carries.
    ///
    /// # Errors
    /// Returns [`ProviderError::Malformed`] for a part with an unknown member, a part that mixes
    /// kinds, and every part kind this adapter cannot express.
    pub(super) fn into_part(self) -> Result<DecodedPart, ProviderError> {
        reject_extra(&self.extra, "parts[]")?;
        if let Some(call) = self.function_call {
            if self.text.is_some()
                || self.inline_data.is_some()
                || self.function_response.is_some()
                || self.file_data.is_some()
            {
                return Err(malformed("a content part carries more than one kind"));
            }
            return call.into_part();
        }
        if self.inline_data.is_some() {
            return Err(malformed(
                "the answer carries inline data, which the canonical stream cannot represent",
            ));
        }
        if self.function_response.is_some() {
            return Err(malformed(
                "the answer carries a tool result, which is a request shape",
            ));
        }
        if self.file_data.is_some() {
            return Err(malformed(
                "the answer references a stored file, which this adapter cannot express",
            ));
        }
        let Some(text) = self.text else {
            return Err(malformed(
                "a content part carries nothing this adapter can read",
            ));
        };
        // The signature of a thought has no canonical event (see the module documentation), so it is
        // validated by being a string and then dropped rather than written into the answer as text.
        let _ = self.thought_signature;
        Ok(if self.thought.unwrap_or(false) {
            DecodedPart::Thought(text)
        } else {
            DecodedPart::Text(text)
        })
    }
}

impl FunctionCallIn {
    /// Turns one call into the canonical increment it carries.
    fn into_part(self) -> Result<DecodedPart, ProviderError> {
        reject_extra(&self.extra, "parts[].functionCall")?;
        let arguments = match self.args {
            Some(args) if !args.is_object() => {
                return Err(malformed("a tool call's arguments are not a JSON object"));
            }
            Some(args) => args,
            // The protocol documents `args` as the call's parameters and sends `{}` when it has none;
            // an absent member therefore means the same thing, not "unknown arguments".
            None => RawJson::new(serde_json::json!({})),
        };
        Ok(DecodedPart::Call {
            id: self.id,
            name: self.name,
            arguments,
        })
    }
}
