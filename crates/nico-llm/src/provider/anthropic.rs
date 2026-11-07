//! The Anthropic provider adapter: canonical values ↔ the Messages wire format.
//!
//! This is one *provider* adapter, not a client codec. It is handed the compiled route facts and a
//! canonical request and answers with an [`Endpoint`] and a body; it reads the provider's response
//! document or event stream back into canonical values. It never learns which client protocol the
//! caller spoke.
//!
//! # What is mapped
//!
//! | Canonical | Messages |
//! |---|---|
//! | `ChatRequest.model` through [`ProviderRoute::model`] | `model` (the physical name) |
//! | leading system/developer `Item::Message` | the top-level `system` text blocks ([`Role`](crate::schema::Role) has no `developer` here) |
//! | user `Item::Message` | one `role: "user"` entry with `text` and `image` blocks |
//! | a run of assistant `Item::Message` + `Item::ToolCall` | one `role: "assistant"` entry with `text` and `tool_use` blocks |
//! | `Item::ToolResult` | `tool_result` blocks at the head of the next user entry (a run of results is one entry) |
//! | `Tool` / `ToolChoice` | `tools[]` with `input_schema` / `tool_choice` (`auto`/`any`/`tool`/`none`) |
//! | `Sampling` | `temperature`, `top_p`, `stop_sequences`, and `max_tokens` |
//! | `ChatResponse` / `StreamEvent` | the message document / its `message_start` … `message_stop` events |
//! | `CanonicalError` | the error envelope a client codec renders |
//!
//! The conversation is rebuilt rather than copied: the canonical model keeps text and tool calls as
//! separate items while this protocol carries them in one message, and tool results belong to a
//! *user* turn even though the canonical item that produced them came after an assistant one.
//!
//! # The output ceiling is endpoint policy
//!
//! `/v1/messages` requires `max_tokens`; the canonical [`Sampling`](crate::schema::Sampling) makes
//! it optional. Inventing a constant would silently cap the caller's answer, so the value comes from
//! the route: `providers[].max_output_tokens` is the ceiling this endpoint uses when the request names
//! none, and a route of this kind without it is refused at publication
//! ([`EndpointError::RouteFact`](super::EndpointError::RouteFact)). A ceiling the caller named always
//! wins; this adapter never rewrites the canonical request.
//!
//! # What the wire cannot express
//!
//! Nothing is dropped silently. A known shape with no field here is refused with a typed error:
//!
//! - `Sampling.seed`, `presence_penalty` and `frequency_penalty` have no field. They change the
//!   answer, so omitting them would answer a different question; each is
//!   [`ProviderError::UnsupportedField`] (`422`). `temperature` above 1.0 is refused the same way
//!   rather than clamped: this protocol's own range is narrower than the canonical one, and writing
//!   a smaller temperature changes the output the caller asked for.
//! - `ResponseFormat`: this protocol has no JSON mode, so `json_object` and `json_schema` are
//!   [`ProviderError::Unsupported`] for the response-format axis, and
//!   [`Provider::capabilities`] does not declare it.
//! - A `ContentPart::File` has no upload field; `files` is not declared.
//! - An `Item::Reasoning` cannot be sent back. Writing a `thinking` block requires enabling thinking
//!   for the request, and the canonical model has no budget knob; a block written without it is
//!   refused upstream. That is why [`Provider::capabilities`] does not declare `reasoning`, while the
//!   *decode* direction still renders a `thinking` block as
//!   [`Reasoning`](crate::schema::Reasoning): a response may carry one, and refusing to read it would
//!   hide part of the answer.
//! - An image is expressible in a user turn only; in a system or assistant turn it is
//!   [`ProviderError::UnsupportedField`] naming `message.content`.
//!
//! A failed tool result ([`ToolResult::is_error`](crate::schema::ToolResult::is_error)) *does* have a
//! field here (`is_error`) and is written; only a shape the protocol cannot carry is refused.
//!
//! # Endpoint
//!
//! The endpoint is the route's `base_url` plus `/v1/messages` (or `<prefix>/messages` when the URL
//! names a prefix), with `content-type`, `accept`, `anthropic-version` and, when the route presents
//! one, `x-api-key`. The data plane owns `host`, `content-length`, `transfer-encoding` and every
//! hop-by-hop header. The `anthropic-version` value is [`VERSION`] and is reported through
//! [`Provider::version`] as well, so the revision the process was assembled with is visible before a
//! request runs.
//!
//! # Failures
//!
//! A non-2xx response becomes [`ProviderError::Upstream`] carrying the canonical error built by the
//! status table in this adapter's `error` module; the provider's `error.type` is kept only as a
//! bounded log token, and its message, body and headers never reach the caller. A 2xx body this
//! adapter cannot read is
//! [`ProviderError::Malformed`], whose diagnostic names a field or a parse position, never the
//! payload.
//!
//! # Capabilities
//!
//! Streaming, tools and images are declared because each is mapped in both directions. `files`,
//! `reasoning` and `response_format` are not, for the reasons above.

use std::collections::BTreeMap;

use super::{Endpoint, Provider, ProviderError, ProviderKind, ProviderRoute, ProviderStream};
use crate::{
    protocol::{Capabilities, Capability, ProtocolId, ResponseFacts},
    schema::{CanonicalError, ChatRequest, ChatResponse, FinishReason, RawJson, SchemaError},
    wire::json,
};

mod error;
mod request;
mod response;
mod stream;
mod url;
mod usage;

/// The provider kind this adapter registers under.
pub const PROVIDER_KIND: ProviderKind = ProviderKind::new("anthropic");

/// The wire protocol this provider speaks natively.
///
/// The value is the client codec's protocol identifier as well, because a `passthrough` route is
/// legal only when both sides name the same protocol: the route's `protocol` is compared with this
/// value at publication, and equality is the whole rule.
pub const NATIVE_PROTOCOL: ProtocolId = ProtocolId::new("anthropic.messages");

/// The wire revision this adapter is pinned to.
///
/// It is sent as `anthropic-version` and reported by [`Provider::version`]; one constant, so the
/// header and the assembly log cannot name different revisions.
pub const VERSION: &str = "2023-06-01";

/// Most content blocks one response may have open at once.
///
/// The stream decoder retains an entry per open block for the life of the response, and the peer picks
/// the `index` of every block, so the number of entries is the peer's choice. A stream is exempt from
/// `max_response_bytes` because nothing holds its body whole — but that exemption is about the body,
/// not about the state a decoder accumulates while reading it, so the accumulator carries its own
/// bound. The value is a count because the bytes are already bounded per event by the SSE framing.
pub const MAX_CONTENT_BLOCKS: u32 = 128;

/// The adapter for Anthropic's Messages wire format.
///
/// The value holds configuration only and no per-request state, so one instance serves every route
/// of this kind and a stream's partial frame lives in the decoder created for that response.
#[derive(Debug, Clone, Copy, Default)]
pub struct AnthropicProvider;

impl AnthropicProvider {
    /// Builds the adapter.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Provider for AnthropicProvider {
    fn kind(&self) -> ProviderKind {
        PROVIDER_KIND
    }

    fn native_protocol(&self) -> ProtocolId {
        NATIVE_PROTOCOL
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::none()
            .with(Capability::Streaming)
            .with(Capability::Tools)
            .with(Capability::Images)
    }

    fn version(&self) -> Option<&'static str> {
        Some(VERSION)
    }

    fn endpoint(&self, route: &ProviderRoute) -> Result<Endpoint, ProviderError> {
        url::endpoint(route).map_err(|source| ProviderError::InvalidEndpoint {
            kind: PROVIDER_KIND,
            source,
        })
    }

    fn encode(
        &self,
        route: &ProviderRoute,
        request: &ChatRequest,
    ) -> Result<Vec<u8>, ProviderError> {
        request::encode(route, request)
    }

    fn decode_response(
        &self,
        head: &ResponseFacts,
        body: &[u8],
    ) -> Result<ChatResponse, ProviderError> {
        response::decode(head, body)
    }

    fn error_for_status(&self, status: u16) -> CanonicalError {
        error::failure(status, b"")
    }

    fn stream_decoder(&self) -> Box<dyn ProviderStream> {
        Box::new(stream::Decoder::new())
    }
}

/// Reports a canonical request this wire format cannot encode.
fn invalid_request(reason: impl Into<String>) -> ProviderError {
    ProviderError::InvalidRequest {
        kind: PROVIDER_KIND,
        reason: reason.into(),
    }
}

/// Reports a canonical request needing something this adapter does not declare.
fn unsupported(feature: Capability) -> ProviderError {
    ProviderError::Unsupported {
        kind: PROVIDER_KIND,
        feature,
    }
}

/// Reports a canonical value this wire format cannot carry where it appears.
///
/// The capability set names features, not positions: an image is expressible in a user turn and not
/// in a system or assistant one, and no capability axis says "images here".
fn unsupported_field(field: &'static str) -> ProviderError {
    ProviderError::UnsupportedField {
        kind: PROVIDER_KIND,
        field: field.into(),
    }
}

/// Reports provider bytes this adapter cannot read.
fn malformed(reason: impl Into<String>) -> ProviderError {
    ProviderError::Malformed {
        kind: PROVIDER_KIND,
        reason: reason.into(),
    }
}

/// Reports a canonical value that broke its own invariant.
///
/// Every canonical constructor validates, so this means the adapter built a value that skipped one:
/// a defect in this crate, not in the provider's document. The message is a log diagnostic; the
/// exchange renders a client-facing failure from its own constant table.
fn schema_failure(error: SchemaError) -> ProviderError {
    malformed(format!(
        "the document breaks a canonical invariant: {error}"
    ))
}

/// Parses one JSON document, reporting only the error class and the position.
fn parse<T: serde::de::DeserializeOwned>(body: &[u8], what: &str) -> Result<T, ProviderError> {
    json::parse(body, what).map_err(|error| malformed(error.to_string()))
}

/// Reports a document whose shape this adapter fully types and does not extend.
///
/// The key is the provider's, so it is quoted only when [`crate::diagnostic::quoted`] allows it; a
/// member whose name cannot be printed is still refused, just not named.
fn reject_extra(extra: &BTreeMap<String, RawJson>, path: &str) -> Result<(), ProviderError> {
    json::reject_extra(extra, path).map_err(|error| malformed(error.reason()))
}

/// Returns the canonical finish reason for its wire spelling.
///
/// The protocol's `pause_turn` means the server-side tool loop paused and the caller should continue
/// it; this gateway has no server tools, so saying `stop` would tell the caller the model finished
/// naturally when it did not. A reason the canonical model has no value for is refused instead.
fn finish_reason(name: &str) -> Result<FinishReason, ProviderError> {
    match name {
        "end_turn" | "stop_sequence" => Ok(FinishReason::Stop),
        "max_tokens" => Ok(FinishReason::Length),
        "tool_use" => Ok(FinishReason::ToolCalls),
        "refusal" => Ok(FinishReason::ContentFilter),
        _ => Err(malformed(
            "the answer reports a finish reason this adapter does not recognize",
        )),
    }
}
