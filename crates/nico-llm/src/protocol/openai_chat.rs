//! The OpenAI Chat Completions client codec: its wire documents ↔ the canonical model.
//!
//! This is a *client* codec: it reads a `/v1/chat/completions` request body and writes the response,
//! stream and error envelopes a caller of that protocol expects. It never learns which provider
//! serves the route; the canonical model is the only vocabulary it shares with one.
//!
//! # What happens to a field this codec does not model
//!
//! A wire field is in exactly one of three states, and the table is closed on purpose — "somewhere
//! else" is how a field silently disappears:
//!
//! 1. **Modelled.** It becomes part of the canonical request.
//! 2. **Preserved.** A top-level field this codec leaves untyped — one it has no arm for, including
//!    a label or a provider preference the canonical request cannot act on — is carried in
//!    [`ChatRequest::extensions`] and written back by an adapter that can express it. The canonical
//!    model does not need to type a field to keep it.
//! 3. **Refused.** A field whose semantics the canonical model cannot carry is refused with
//!    [`CodecError::UnsupportedField`] (a request-time `422`), never dropped or approximated. The
//!    same applies to a field whose *value* asks for something the canonical model cannot carry
//!    (`n > 1`, log-probabilities, audio output), and to a known field inside a modelled object that
//!    has no canonical home (`tools[].function.strict`, `image_url.detail`).
//!
//! A field that is *not* part of this protocol's document at all — an unknown key inside a message,
//! a typo in `tools[]` — is a malformed document and is refused with
//! [`CodecError::InvalidRequest`], naming the field path. Neither refusal ever carries the field's
//! value: a prompt, an image URL and a tool argument are all caller data.
//!
//! # Capabilities
//!
//! The codec declares what it actually maps: streaming, tools, images, reasoning and a structured
//! response format. Files are *not* declared: this document can reference a file by id, and the
//! canonical model carries inline file bytes, so there is no truthful mapping yet.

use std::collections::BTreeMap;

use crate::{
    protocol::{
        Capabilities, Capability, ClientCodec, ClientReply, ClientStreamEncoder, CodecError,
        ProtocolId, RequestFacts,
    },
    schema::{CanonicalError, ChatRequest, ChatResponse, FinishReason, RawJson, SchemaError},
};

mod declaration;
mod error;
mod message;
mod request;
mod response;
mod stream;
mod usage;

/// The protocol identifier this codec registers under.
pub const PROTOCOL: ProtocolId = ProtocolId::new("openai.chat");

/// The client codec for OpenAI's Chat Completions document.
///
/// The value holds no state: one instance serves every route that speaks the protocol, and the state
/// one streamed response needs lives in the encoder it creates per response.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenAiChatCodec;

impl OpenAiChatCodec {
    /// Builds the codec.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl ClientCodec for OpenAiChatCodec {
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

    fn decode_request(
        &self,
        _facts: &RequestFacts,
        body: &[u8],
    ) -> Result<ChatRequest, CodecError> {
        // The exchange is a `POST` to a route-configured path by construction, and no part of this
        // document depends on the method, the path or a header: the body is the whole request.
        request::decode(body)
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
fn invalid(reason: impl Into<String>) -> CodecError {
    CodecError::InvalidRequest {
        protocol: PROTOCOL,
        reason: reason.into(),
    }
}

/// Refuses a known wire field the canonical model cannot carry.
fn unsupported(field: impl Into<String>) -> CodecError {
    CodecError::UnsupportedField {
        protocol: PROTOCOL,
        field: field.into(),
    }
}

/// Refuses a request needing a capability this codec does not have.
fn unsupported_capability(feature: Capability) -> CodecError {
    CodecError::Unsupported {
        protocol: PROTOCOL,
        feature,
    }
}

/// Refuses the first key of an object whose shape this codec fully types.
fn reject_extra(extra: &BTreeMap<String, RawJson>, path: &str) -> Result<(), CodecError> {
    match extra.keys().next() {
        Some(key) => Err(invalid(format!(
            "field `{path}.{key}` is not part of this document"
        ))),
        None => Ok(()),
    }
}

/// Reports a canonical invariant the wire value broke, naming the field.
fn schema(error: SchemaError) -> CodecError {
    invalid(error.to_string())
}

/// Reports a canonical invariant broken by one named field.
fn field(name: &str, error: SchemaError) -> CodecError {
    invalid(format!("field `{name}` is invalid: {error}"))
}

/// The `object` field of a complete answer.
const OBJECT: &str = "chat.completion";

/// The `object` field of one streamed chunk.
const CHUNK_OBJECT: &str = "chat.completion.chunk";

/// The `created` timestamp an encoded answer carries.
///
/// The canonical model has no creation time: a provider adapter does not decode one into it, and
/// repeating the provider's value would make a re-encoded answer claim a time this gateway never
/// observed. The encoder therefore writes `0`, the one value that cannot be mistaken for a real
/// timestamp, and keeps the output deterministic for a fixture.
const CREATED: u64 = 0;

/// Returns the wire spelling of a finish reason.
///
/// [`FinishReason::Error`] has no spelling of its own: a failure is carried by the error envelope,
/// not by a successful body, so a turn the provider marked as failed is presented as having stopped.
fn finish_reason(reason: FinishReason) -> &'static str {
    match reason {
        FinishReason::Stop | FinishReason::Error => "stop",
        FinishReason::Length => "length",
        FinishReason::ToolCalls => "tool_calls",
        FinishReason::ContentFilter => "content_filter",
    }
}
