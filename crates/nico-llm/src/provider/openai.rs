//! The OpenAI provider adapter: canonical values ↔ the Chat Completions wire format.
//!
//! This is one *provider* adapter, not a client codec. It is handed the compiled route facts and a
//! canonical request and answers with an [`Endpoint`] and a body; it reads the provider's response
//! document or event stream back into canonical values. It never learns which client protocol the
//! caller spoke, and the client codec never learns which provider serves the route.
//!
//! # What is mapped
//!
//! | Canonical | Chat Completions |
//! |---|---|
//! | `ChatRequest.model` through [`ProviderRoute::model`] | `model` (the physical name) |
//! | `Item::Message` system/developer/user/assistant | one `messages[]` entry per role |
//! | a run of `Item::Reasoning` + assistant `Item::Message` + `Item::ToolCall` | one assistant entry with `reasoning_content`, `content` and `tool_calls` |
//! | `Item::ToolResult` | a `role: "tool"` entry with `tool_call_id` |
//! | `Tool` / `ToolChoice` | `tools[]` / `tool_choice` |
//! | `Sampling` | `temperature`, `top_p`, `max_tokens`, `stop`, `seed`, both penalties |
//! | `ResponseFormat` | `response_format` (`text` is the protocol default and is omitted) |
//! | `ChatResponse` / `StreamEvent` | the completion document / its chunks |
//! | `CanonicalError` | the error envelope a client codec renders |
//!
//! A conversation item run is rebuilt rather than copied: the canonical model keeps reasoning, text
//! and tool calls as separate items, while this protocol carries them in one assistant message. The
//! assistant run is flushed when an item of another role arrives, so the wire order is the canonical
//! order.
//!
//! # What the wire cannot express
//!
//! Nothing is dropped silently. A known shape with no field here is refused with a typed error, and
//! the module documents the refusal so a later protocol matrix knows why a combination is missing:
//!
//! - A content part [`ContentPart::File`](crate::schema::ContentPart::File) has no upload path in
//!   this adapter, so `files` is not a declared capability and an encoder that meets one reports
//!   [`ProviderError::Unsupported`] for it.
//! - The protocol carries content per role: an image is expressible in a user turn and nowhere else,
//!   a [`Reasoning`](crate::Reasoning) item with a provider signature cannot be sent back (the
//!   signature is what lets the issuing provider verify the block, and writing only the text would
//!   silently change the conversation), and a failed tool result
//!   ([`ToolResult::is_error`](crate::schema::ToolResult::is_error)) has no field. These are
//!   [`ProviderError::UnsupportedField`], which names the canonical field and the exchange reports as
//!   `unsupported_capability` (`422`): the document is well formed and this route cannot serve it.
//!   A capability axis would not fit — the axes name features, not positions — and answering `500`
//!   would blame the gateway for a request it understood. A later client protocol that can produce
//!   one of these shapes gets the same refusal; it does not need a new decision here.
//! - The OpenAI client codec already accepts an image part in any role, so the image refusal is
//!   reachable from a document that codec accepts even though the provider cannot frame it. That is
//!   the case the `422` is for.
//!
//! # Endpoint
//!
//! The endpoint resolver turns the route's `base_url` into a destination and the chat path; the
//! endpoint's headers are `content-type`, `accept` and, when the route presents one, `authorization`.
//! The data plane owns `host`, `content-length`, `transfer-encoding` and every hop-by-hop header, so
//! the adapter never sets them. An `https` base URL is a valid endpoint here even though this build
//! cannot speak TLS: refusing it is the data plane's decision, not the adapter's.
//!
//! The host a base URL names is [`nico_core::validate_host`]'s rule — the same one a document's
//! `upstreams[].host` is held to — so a destination never carries a value the data plane would write
//! as an unreadable `Host`. That rule has no room for a colon, so an IPv6 literal is refused: a
//! destination holds a bare name and the data plane writes `host:port` without re-bracketing.
//!
//! The adapter does not fail when a route presents no credential. A local compatible endpoint needs
//! none, and an empty credential is a publication concern (the materializer refuses it), not
//! something an adapter should guess about.
//!
//! # Failures
//!
//! **Every failure is typed and named.** A non-2xx response becomes [`ProviderError::Upstream`]
//! carrying a [`CanonicalError`] built by the status table: the
//! provider's status selects the category, stable code and client-facing status, and only its
//! `error.code` (when it is a bounded printable token) is kept, for structured logs. The provider's
//! message, body and headers are not decoded into the client answer. A 2xx body this adapter cannot
//! read is [`ProviderError::Malformed`]; its diagnostic names a field or a parse position, never the
//! payload, because the payload is the model's answer.
//!
//! # Capabilities
//!
//! Streaming, tools, images, reasoning and a structured response format are declared because each is
//! mapped. `files` is not declared: a canonical file part carries inline bytes, and this protocol
//! addresses files it already stores rather than accepting bytes in a chat request. A remote image is
//! sent as an `image_url` part; an inline image is sent as a `data:` URL, so the `images`
//! declaration covers both canonical spellings.

use std::collections::BTreeMap;

use super::{Endpoint, Provider, ProviderError, ProviderKind, ProviderRoute, ProviderStream};
use crate::{
    protocol::{Capabilities, Capability, ProtocolId, ResponseFacts},
    schema::{CanonicalError, ChatRequest, ChatResponse, FinishReason, RawJson, SchemaError},
    wire::json,
};

// The status table and the base-URL operation helper are shared with this vendor's second dialect
// (`provider/openai_responses`). One table, one set of URL rules: a second copy would be a second
// answer to the same question, and the two dialects would drift.
pub(in crate::provider) mod error;
mod request;
mod response;
mod stream;
pub(in crate::provider) mod url;
mod usage;

/// The provider kind this adapter registers under.
pub const PROVIDER_KIND: ProviderKind = ProviderKind::new("openai");

/// Most tool calls one response may have open at once.
///
/// The stream decoder retains an entry per open call for the life of the response, and the peer picks
/// the `index` of every fragment, so the number of entries is the peer's choice. A stream is exempt
/// from `max_response_bytes` because nothing holds its body whole — but that exemption is about the
/// body, not about the state a decoder accumulates while reading it, so the accumulator carries its
/// own bound. The value is a count because the bytes are already bounded per event by the SSE framing.
pub const MAX_TOOL_CALLS: usize = 128;

/// The wire protocol this provider speaks natively.
///
/// A route may serve any client protocol; this value names the provider's own, which publication
/// needs to decide whether a `passthrough` route may skip the canonical model.
pub const NATIVE_PROTOCOL: ProtocolId = ProtocolId::new("openai.chat");

/// The adapter for OpenAI's Chat Completions wire format.
///
/// The value holds configuration only and no per-request state, so one instance serves every route
/// of this kind and a stream's partial frame lives in the decoder created for that response.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenAiProvider;

impl OpenAiProvider {
    /// Builds the adapter.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Provider for OpenAiProvider {
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
            .with(Capability::Reasoning)
            .with(Capability::ResponseFormat)
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
/// in a system, assistant or tool one, and no capability axis says "images here".
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
///
/// The shared helper owns the rule; this is the adapter's mapping of it into a malformed answer.
fn parse<T: serde::de::DeserializeOwned>(body: &[u8], what: &str) -> Result<T, ProviderError> {
    json::parse(body, what).map_err(|error| malformed(error.to_string()))
}

/// Reports a document whose shape this adapter fully types and does not extend.
///
/// The key is the provider's, so it is quoted only when [`crate::diagnostic::quoted`] allows it; a document
/// carrying a member whose name cannot be printed is still refused, just not named.
fn reject_extra(extra: &BTreeMap<String, RawJson>, path: &str) -> Result<(), ProviderError> {
    json::reject_extra(extra, path).map_err(|error| malformed(error.reason()))
}

/// Returns the canonical finish reason for its wire spelling.
///
/// `function_call` is the deprecated spelling of `tool_calls`; a provider that still sends it means
/// the same thing. A reason the canonical model has no value for is refused instead of being mapped
/// to `stop`: telling a caller the model finished naturally when it did not changes what the caller
/// does next.
fn finish_reason(name: &str) -> Result<FinishReason, ProviderError> {
    match name {
        "stop" => Ok(FinishReason::Stop),
        "length" => Ok(FinishReason::Length),
        "tool_calls" | "function_call" => Ok(FinishReason::ToolCalls),
        "content_filter" => Ok(FinishReason::ContentFilter),
        _ => Err(malformed(
            "the answer reports a finish reason this adapter does not recognize",
        )),
    }
}
