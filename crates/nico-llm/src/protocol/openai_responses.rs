//! The OpenAI Responses client codec: its wire documents ↔ the canonical model.
//!
//! This is a *client* codec for `POST /v1/responses`. The model travels in the body, so the request
//! target carries nothing this codec reads. The protocol is the youngest of the four, and the field
//! table behind `provider/openai_responses` (`research/responses-wire.md`) is the same table read in
//! the other direction; a member whose grade there is `refused` is refused here too.
//!
//! # What happens to a field this codec does not model
//!
//! The closed three-state table every codec uses:
//!
//! 1. **Modelled.** It becomes part of the canonical request.
//! 2. **Preserved.** A top-level field with no effect on generation (`metadata`, `user`) is carried in
//!    [`ChatRequest::extensions`].
//! 3. **Refused.** A field whose value would change the answer is
//!    [`CodecError::UnsupportedField`] — a `422`. This protocol has more of them than the others,
//!    because it is stateful and tool-rich: `previous_response_id` (the gateway stores no
//!    conversation), `store: true` (it does not let a provider retain one on the caller's behalf),
//!    `reasoning` (a reasoning item needs *two* replay handles, `id` and `encrypted_content`, and the
//!    canonical model carries one), the built-in tools, `truncation`, `parallel_tool_calls`,
//!    `include`, `text.verbosity`, `service_tier`, `background` and a `prompt` template reference.
//!
//! A refusal names a field path and never its value.
//!
//! # Capabilities
//!
//! Streaming, tools, images and a structured response format are declared because each is mapped.
//! `reasoning` is **not**: see above — an item replayed through this codec would lose the handle that
//! makes it valid, so a request carrying one is refused at the capability gate. `files` is not
//! declared either.

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
mod items;
mod request;
mod response;
mod stream;
mod usage;

/// The protocol identifier this codec registers under.
pub const PROTOCOL: ProtocolId = ProtocolId::new("openai.responses");

/// The client codec for OpenAI's Responses document.
///
/// The value holds no state: one instance serves every route that speaks the protocol, and the state
/// one streamed response needs lives in the encoder it creates per response.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenAiResponsesCodec;

impl OpenAiResponsesCodec {
    /// Builds the codec.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl ClientCodec for OpenAiResponsesCodec {
    fn protocol(&self) -> ProtocolId {
        PROTOCOL
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::none()
            .with(Capability::Streaming)
            .with(Capability::Tools)
            .with(Capability::Images)
            .with(Capability::ResponseFormat)
    }

    fn decode_request(
        &self,
        _facts: &RequestFacts,
        body: &[u8],
    ) -> Result<ChatRequest, CodecError> {
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
fn unsupported(field: impl Into<Cow<'static, str>>) -> CodecError {
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

/// Reports a canonical invariant the wire value broke.
fn schema(error: SchemaError) -> CodecError {
    invalid(error.to_string())
}

/// Reports a canonical invariant broken by one named field.
fn field(name: &str, error: SchemaError) -> CodecError {
    invalid(format!("field `{name}` is invalid: {error}"))
}
