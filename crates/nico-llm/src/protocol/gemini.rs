//! The Gemini `generateContent` client codec: its wire documents ↔ the canonical model.
//!
//! This is a *client* codec for `POST /v1beta/models/{model}:generateContent` and
//! `:streamGenerateContent?alt=sse`. Unlike the other three protocols, this one puts two request
//! facts into the request target instead of the body — the model and the operation — so
//! [`ClientCodec::decode_request`] reads them from [`RequestFacts::path`] (which the data plane
//! hands over with the query string; see the `target` module) and the body carries no `model`
//! member at all. The field table below is the one [`crate::provider::gemini`] reads in the other
//! direction; a member that adapter grades `refused` is refused here too.
//!
//! # What happens to a field this codec does not model
//!
//! The closed three-state table every codec uses:
//!
//! 1. **Modelled.** It becomes part of the canonical request.
//! 2. **Preserved.** A top-level field with no effect on generation (`labels`) is carried in
//!    [`ChatRequest::extensions`].
//! 3. **Refused.** A field whose value would change the answer is [`CodecError::UnsupportedField`] —
//!    a `422`: `candidateCount` above one (the canonical model addresses one candidate),
//!    `safetySettings`, `cachedContent`, `thinkingConfig` (the canonical model has no reasoning
//!    budget), the built-in tools `googleSearch`/`codeExecution`/`urlContext` (they run at the
//!    provider and a gateway can neither call nor replay them), `generationConfig.topK` (the
//!    canonical `Sampling` is frozen; §10) and an `alt` other than `sse`. A `model` member in the
//!    body is refused as a bad document instead of preserved: the path names the model, and a second
//!    one beside it is exactly the silent switch the pinned target exists to prevent.
//!
//! A refusal names a field path and never its value.
//!
//! # Capabilities
//!
//! Streaming, tools, images, reasoning and a structured response format are declared because each is
//! mapped in both directions: a thought part replays as a canonical reasoning item with its
//! signature, and `responseMimeType`/`responseSchema` carry the structured formats. `files` is
//! **not**: this protocol's `fileData` references a stored file, while the canonical model carries
//! inline bytes.
//!
//! # The answer side
//!
//! One complete answer is one `GenerateContentResponse` document, and a streamed answer is one such
//! document per event: this protocol has no delta fields, so the stream encoder writes a text or
//! thought increment as its own chunk and accumulates a tool call's arguments until the canonical
//! stream closes the call. A `finishReason` is always written (`STOP` also after tool calls, which
//! is what this dialect reports there), and usage is written as its own chunk, so a report that
//! arrives after the finish event still lands before the stream ends — this protocol has no
//! terminator frame, the body simply ends.

use std::{borrow::Cow, collections::BTreeMap};

use crate::{
    diagnostic,
    protocol::{
        Capabilities, Capability, ClientCodec, ClientReply, ClientStreamEncoder, CodecError,
        ProtocolId, RequestFacts,
    },
    schema::{CanonicalError, ChatRequest, ChatResponse, RawJson, SchemaError},
};

mod error;
mod parts;
mod request;
mod response;
mod stream;
mod target;
mod usage;

/// The protocol identifier this codec registers under.
pub const PROTOCOL: ProtocolId = ProtocolId::new("gemini.generate_content");

/// The API revision this codec is pinned to: the `v1beta` segment of the request path.
pub const VERSION: &str = "v1beta";

/// The prefix of the call id this dialect assigns when a call names none.
///
/// `functionCall.id` is optional while the canonical [`ToolCallId`](crate::schema::ToolCallId) is
/// required. The assigned id is the prefix plus the call's position among the calls of its
/// document, so it is stable for one exchange and the next request writes it back. Both sides of
/// the dialect assign under one rule, so the provider adapter re-exports this constant rather than
/// spelling a second one.
pub const CALL_ID_PREFIX: &str = "call_";

/// The client codec for Gemini's `generateContent` document.
///
/// The value holds no state: one instance serves every route that speaks the protocol, and the state
/// one streamed response needs lives in the encoder it creates per response.
#[derive(Debug, Clone, Copy, Default)]
pub struct GeminiGenerateContentCodec;

impl GeminiGenerateContentCodec {
    /// Builds the codec.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl ClientCodec for GeminiGenerateContentCodec {
    fn protocol(&self) -> ProtocolId {
        PROTOCOL
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::none()
            .with(Capability::Streaming)
            .with(Capability::Tools)
            .with(Capability::Images)
            .with(Capability::Reasoning)
            .with(Capability::ResponseFormat)
    }

    fn version(&self) -> Option<&'static str> {
        Some(VERSION)
    }

    fn decode_request(&self, facts: &RequestFacts, body: &[u8]) -> Result<ChatRequest, CodecError> {
        request::decode(facts, body)
    }

    fn encode_response(&self, response: &ChatResponse) -> Result<ClientReply, CodecError> {
        response::encode(response)
    }

    fn stream_encoder(&self) -> Box<dyn ClientStreamEncoder> {
        Box::new(stream::Encoder::new())
    }

    fn encode_error(&self, error: &CanonicalError) -> Result<Vec<u8>, CodecError> {
        error::encode(error)
    }
}

/// Reports a document this codec cannot read, naming the field that failed.
pub(super) fn invalid(reason: impl Into<String>) -> CodecError {
    CodecError::InvalidRequest {
        protocol: PROTOCOL,
        reason: reason.into(),
    }
}

/// Refuses a known wire field the canonical model cannot carry.
pub(super) fn unsupported(field: impl Into<Cow<'static, str>>) -> CodecError {
    CodecError::UnsupportedField {
        protocol: PROTOCOL,
        field: field.into(),
    }
}

/// Refuses a request needing a capability this codec does not have.
pub(super) fn unsupported_capability(feature: Capability) -> CodecError {
    CodecError::Unsupported {
        protocol: PROTOCOL,
        feature,
    }
}

/// Reports a canonical invariant the wire value broke.
pub(super) fn schema(error: SchemaError) -> CodecError {
    invalid(error.to_string())
}

/// Reports a canonical invariant broken by one named field.
pub(super) fn field(name: &str, error: SchemaError) -> CodecError {
    invalid(format!("field `{name}` is invalid: {error}"))
}

/// Builds the dotted path of a member whose name comes from the document.
///
/// The key is the caller's, so it is quoted only when [`diagnostic::quoted`] allows it; a member
/// whose name cannot be printed is still refused, just under the bare path. The result is owned in
/// both arms, because a field path travels in an error whose field outlives the document.
pub(super) fn named(path: &str, key: &str) -> Cow<'static, str> {
    match diagnostic::quoted(key, diagnostic::MAX_QUOTED_FIELD_BYTES) {
        Some(key) => Cow::Owned(format!("{path}.{key}")),
        None => Cow::Owned(path.to_owned()),
    }
}

/// Reports the first key of an object whose shape this codec fully types.
pub(super) fn reject_extra(
    extra: &BTreeMap<String, RawJson>,
    path: &str,
) -> Result<(), CodecError> {
    let Some(key) = extra.keys().next() else {
        return Ok(());
    };
    Err(
        match diagnostic::quoted(key, diagnostic::MAX_QUOTED_FIELD_BYTES) {
            Some(key) => invalid(format!("field `{path}.{key}` is not part of this document")),
            None => invalid(format!(
                "`{path}` carries a field this document does not model"
            )),
        },
    )
}
