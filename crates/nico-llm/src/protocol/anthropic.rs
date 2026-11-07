//! The Anthropic Messages client codec: its wire documents ↔ the canonical model.
//!
//! This is a *client* codec: it reads a `/v1/messages` request body and writes the response, stream
//! and error documents a caller of that protocol expects. It never learns which provider serves the
//! route. The model name travels in the body, so the request target carries nothing this codec reads.
//!
//! # What happens to a field this codec does not model
//!
//! The same closed three-state table every codec uses:
//!
//! 1. **Modelled.** It becomes part of the canonical request.
//! 2. **Preserved.** A top-level field with no effect on generation (`metadata`, `anthropic_beta`) is
//!    carried in [`ChatRequest::extensions`] and written back by an adapter that can express it.
//! 3. **Refused.** A field the canonical model cannot carry is refused, never dropped. Two grades:
//!    a *known* field whose value would change the answer (`thinking`, `top_k`, `service_tier`,
//!    `mcp_servers`, `container`, `context_management`) is
//!    [`CodecError::UnsupportedField`] — a request-time `422`; a value outside this protocol's own
//!    range (`temperature > 1.0`) or a malformed document (missing `max_tokens`, an unknown block
//!    type) is [`CodecError::InvalidRequest`] — a `400`.
//!
//! A refusal names a field path and never its value: a prompt, an image URL and a tool argument are
//! all caller data.
//!
//! # Capabilities
//!
//! Streaming, tools, images and reasoning are declared because each is mapped. `response_format` is
//! **not**: this protocol has no JSON mode, so a structured-output request is a `422` rather than a
//! silently ignored field. `files` is not declared either — the canonical file part carries inline
//! bytes and this protocol addresses documents it is given, not one it stores.
//!
//! # The two content shapes
//!
//! This protocol writes a message's content as a string when it is a single text block and as an
//! array otherwise; it reads both. The encoder always writes the array, because a response may mix
//! text, thinking and tool calls and one spelling keeps a fixture from having two forms.

use std::{borrow::Cow, collections::BTreeMap};

use crate::{
    diagnostic,
    protocol::{
        Capabilities, Capability, ClientCodec, ClientReply, ClientStreamEncoder, CodecError,
        ProtocolId, RequestFacts,
    },
    schema::{CanonicalError, ChatRequest, ChatResponse, FinishReason, RawJson, SchemaError},
};

mod blocks;
mod error;
mod request;
mod response;
mod stream;
mod usage;

/// The protocol identifier this codec registers under.
pub const PROTOCOL: ProtocolId = ProtocolId::new("anthropic.messages");

/// The dated revision of the Messages document this codec is pinned to.
///
/// The value is what a caller sends in `anthropic-version`, and the assembly log reports it per
/// process: an operator reading the line learns which spelling this build speaks without a request.
pub const VERSION: &str = "2023-06-01";

/// The client codec for Anthropic's Messages document.
///
/// The value holds no state: one instance serves every route that speaks the protocol, and the state
/// one streamed response needs lives in the encoder it creates per response.
#[derive(Debug, Clone, Copy, Default)]
pub struct AnthropicMessagesCodec;

impl AnthropicMessagesCodec {
    /// Builds the codec.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl ClientCodec for AnthropicMessagesCodec {
    fn protocol(&self) -> ProtocolId {
        PROTOCOL
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::none()
            .with(Capability::Streaming)
            .with(Capability::Tools)
            .with(Capability::Images)
            .with(Capability::Reasoning)
    }

    fn version(&self) -> Option<&'static str> {
        Some(VERSION)
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
///
/// The field names are literals, and a decode path is built from the position it stands at, so the
/// payload is a [`Cow`]: borrowed for the fixed names, owned for the computed ones.
fn unsupported(field: impl Into<Cow<'static, str>>) -> CodecError {
    CodecError::UnsupportedField {
        protocol: PROTOCOL,
        field: field.into(),
    }
}

/// Refuses the first key of an object whose shape this codec fully types.
///
/// The key is the caller's, so it is quoted only when [`diagnostic::quoted`] allows it; a field whose
/// name cannot be printed is still refused, just not named.
fn reject_extra(extra: &BTreeMap<String, RawJson>, path: &str) -> Result<(), CodecError> {
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

/// Reports a canonical invariant the wire value broke, naming the field.
fn schema(error: SchemaError) -> CodecError {
    invalid(error.to_string())
}

/// Reports a canonical invariant broken by one named field.
fn field(name: &str, error: SchemaError) -> CodecError {
    invalid(format!("field `{name}` is invalid: {error}"))
}

/// Returns the wire spelling of a finish reason.
///
/// [`FinishReason::Error`] has no spelling of its own in this protocol: a failure is carried by the
/// error envelope, not by a successful body, so a turn the provider marked as failed is presented as
/// having stopped. [`FinishReason::ContentFilter`] is `refusal`, which is the word this protocol uses
/// for output a policy cut short.
fn finish_reason(reason: FinishReason) -> &'static str {
    match reason {
        FinishReason::Stop | FinishReason::Error => "end_turn",
        FinishReason::Length => "max_tokens",
        FinishReason::ToolCalls => "tool_use",
        FinishReason::ContentFilter => "refusal",
    }
}
