//! The OpenAI Responses provider adapter: canonical values ↔ the `/v1/responses` wire format.
//!
//! This is one *provider* adapter, not a client codec. It is handed the compiled route facts and a
//! canonical request and answers with an [`Endpoint`] and a body; it reads the provider's response
//! document or event stream back into canonical values. It never learns which client protocol the
//! caller spoke.
//!
//! # Two dialects, two kinds
//!
//! OpenAI publishes two request paths that are not the same wire format: Chat Completions
//! (`provider::openai`) and Responses. They share a vendor, an error envelope and the base-URL rules,
//! and nothing else: the request documents, the response documents and the event names all differ. A
//! provider kind names *one dialect*, so this adapter is registered as `openai_responses`, its
//! [`Provider::native_protocol`] is `openai.responses`, and a `passthrough` route is legal only when a
//! client codec names that same protocol. An operator who wants both dialects writes two provider
//! entries, with their own base URLs and credentials — the alternative (one kind that picks a dialect
//! from the client protocol) would make the provider's wire format depend on the caller's, and would
//! make `passthrough` legality a set membership instead of equality.
//!
//! # What is mapped
//!
//! | Canonical | Responses |
//! |---|---|
//! | `ChatRequest.model` through [`ProviderRoute::model`] | `model` (the physical name) |
//! | leading system/developer `Item::Message` | one merged `instructions` string |
//! | user `Item::Message` | an `input` message with `input_text` and `input_image` parts |
//! | assistant `Item::Message` | an `input` message whose text parts are `output_text` |
//! | `Item::ToolCall` | `{type:"function_call", call_id, name, arguments}` |
//! | `Item::ToolResult` | `{type:"function_call_output", call_id, output}` |
//! | `Tool` / `ToolChoice` | `tools[]` (flat function shape) / `tool_choice` |
//! | `Sampling` | `temperature`, `top_p`, `max_output_tokens` |
//! | `ResponseFormat` | `text.format` |
//! | `ChatResponse` | the `output` item list, `status` and `usage` |
//! | `StreamEvent` | the `response.*` events |
//!
//! The conversation is written one item per canonical item, because this dialect's `input` *is* an
//! item list: unlike Chat Completions it needs no assistant-run folding, and unlike Messages a tool
//! result needs no grouping. The one reshape is the instruction field, which is a single string here
//! and therefore has no position for a system turn that appears after the conversation started.
//!
//! # What the wire cannot express
//!
//! Nothing is dropped silently. A canonical value with no field here is a typed refusal, and the field
//! table in `research/responses-wire.md` records every decision:
//!
//! - `Item::Reasoning` cannot be sent back. Replaying a reasoning item needs **two** handles
//!   (`id` and `encrypted_content`) and the canonical [`Reasoning`](crate::schema::Reasoning) has one
//!   opaque `signature`; writing the text alone would change the conversation, and writing one handle
//!   and not the other produces a request the upstream refuses. The *decode* direction still renders a
//!   reasoning item's summary — refusing to read part of the answer would hide it — which is why this
//!   adapter declares no `reasoning` capability while it still reports one.
//! - `Sampling.stop`, `seed` and the two penalties have no field in this dialect's request object.
//!   They change the answer, so omitting them would answer a different question.
//! - `ResponseFormat` *is* expressible (`text.format`), so the capability is declared.
//! - A `ContentPart::File` has no field here: this dialect addresses stored files, and the canonical
//!   model carries inline bytes. `files` is not declared.
//! - A failed tool result ([`ToolResult::is_error`](crate::schema::ToolResult::is_error)) has no field
//!   on a `function_call_output`. The refusal names the canonical field: the exchange answers `422`
//!   `unsupported_capability`, because the document is well formed and this route cannot serve it.
//! - An image is expressible in a user turn only. An assistant turn's content is `output_text`, so an
//!   image there is refused by the canonical field it occupies (`message.content`).
//!
//! # Members consumed rather than preserved
//!
//! This dialect's response object echoes the request — `instructions`, `tools`, `text`, `temperature`
//! and the rest — beside the answer. Those members describe the request, not the answer, and the
//! canonical [`Extensions`](crate::schema::Extensions) is written back into a *client's* document by a
//! client codec, where an echo would either be a fact the caller never asked to relay or collide with
//! the codec's own field. They are therefore consumed from one documented list (`response::ENVELOPE`)
//! and only a member this adapter does not know is preserved. The exact echo set is the one
//! unverified item of the field table; the list is a superset of the members this adapter reads.
//!
//! # Endpoint
//!
//! The endpoint is the route's `base_url` plus `/v1/responses` (or `<prefix>/responses` when the URL
//! names a prefix), with `content-type`, `accept` and, when the route presents one,
//! `authorization: Bearer …`. The data plane owns `host`, `content-length`, `transfer-encoding` and
//! every hop-by-hop header. The path pair and the header set come from the chat dialect's
//! `provider::openai::url`, which is the same rule set: only the two path constants are this
//! dialect's.
//!
//! `max_output_tokens` is optional in this dialect, so — unlike the Messages adapter — no route fact
//! is required and a request that names no ceiling simply omits the field.
//!
//! # Failures
//!
//! A non-2xx response becomes [`ProviderError::Upstream`] carrying the canonical error built by the
//! shared status table; the provider's `error.code` is kept only as a bounded log token, and its
//! message, body and headers never reach the caller. A 2xx body this adapter cannot read is
//! [`ProviderError::Malformed`], whose diagnostic names a field or a parse position, never the
//! payload. A `response.failed` frame and an `error` event become
//! [`StreamEvent::Error`](crate::schema::StreamEvent::Error) with the same treatment.
//!
//! # Capabilities
//!
//! Streaming, tools, images and a structured response format are declared because each is mapped in
//! both directions. `reasoning` is not (see above) and neither is `files`.

use std::collections::BTreeMap;

use super::{Endpoint, Provider, ProviderError, ProviderKind, ProviderRoute, ProviderStream};
use crate::{
    protocol::{Capabilities, Capability, ProtocolId, ResponseFacts},
    schema::{CanonicalError, ChatRequest, ChatResponse, FinishReason, RawJson, SchemaError},
    wire::json,
};

mod request;
mod response;
mod stream;
mod usage;

/// The provider kind this adapter registers under.
pub const PROVIDER_KIND: ProviderKind = ProviderKind::new("openai_responses");

/// The wire protocol this provider speaks natively.
///
/// A client codec that speaks this dialect names itself the same way, because a `passthrough` route is
/// legal only when both sides name the same protocol: equality is the whole rule.
pub const NATIVE_PROTOCOL: ProtocolId = ProtocolId::new("openai.responses");

/// The Responses path of a base URL that names no path of its own.
const DEFAULT_REQUEST_PATH: &str = "/v1/responses";

/// The path appended to a base URL that names an API prefix.
const RESPONSES_PATH: &str = "/responses";

/// Most output items one streamed response may have open at once.
///
/// The decoder retains an entry per announced item for the life of the response, and the peer picks
/// every `output_index`, so the number of entries is the peer's choice. A stream is exempt from
/// `max_response_bytes` because nothing holds its body whole — but that exemption is about the body,
/// not about the state a decoder accumulates while reading it, so the accumulator carries its own
/// bound. The value is a count because the bytes are already bounded per event by the SSE framing.
pub const MAX_OUTPUT_ITEMS: u32 = 128;

/// The adapter for OpenAI's Responses wire format.
///
/// The value holds configuration only and no per-request state, so one instance serves every route of
/// this kind and a stream's partial frame lives in the decoder created for that response.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenAiResponsesProvider;

impl OpenAiResponsesProvider {
    /// Builds the adapter.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Provider for OpenAiResponsesProvider {
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
            .with(Capability::ResponseFormat)
    }

    fn endpoint(&self, route: &ProviderRoute) -> Result<Endpoint, ProviderError> {
        // The path pair is this dialect's; the parsing rules, the prefix join and the header set are
        // the chat dialect's, and they are the same for both paths.
        super::openai::url::operation(route, DEFAULT_REQUEST_PATH, RESPONSES_PATH).map_err(
            |source| ProviderError::InvalidEndpoint {
                kind: PROVIDER_KIND,
                source,
            },
        )
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
        super::openai::error::failure(status, b"")
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
/// in an assistant one, and no capability axis says "images here".
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
/// document carrying a member whose name cannot be printed is still refused, just not named.
fn reject_extra(extra: &BTreeMap<String, RawJson>, path: &str) -> Result<(), ProviderError> {
    json::reject_extra(extra, path).map_err(|error| malformed(error.reason()))
}

/// Returns the canonical finish reason of an answer's status.
///
/// The status and the incomplete reason are two facts, and neither alone decides the answer:
/// `incomplete` says generation stopped early but not why, and a `completed` answer may still have
/// stopped to call a tool. `failed` is an answer-shaped failure the caller reads in its own protocol,
/// not a gateway error — the provider served the request and reported that generation failed.
///
/// A status that does not describe a finished answer (`in_progress`, `queued`, `cancelled`, or one
/// this adapter does not know) is refused rather than mapped to `stop`: telling a caller the model
/// finished naturally when nobody said it did changes what the caller does next.
fn finish_reason(
    status: &str,
    incomplete: Option<&str>,
    has_calls: bool,
) -> Result<FinishReason, ProviderError> {
    match status {
        "completed" if has_calls => Ok(FinishReason::ToolCalls),
        "completed" => Ok(FinishReason::Stop),
        "incomplete" => match incomplete {
            Some("max_output_tokens") => Ok(FinishReason::Length),
            Some("content_filter") => Ok(FinishReason::ContentFilter),
            _ => Err(malformed(
                "the answer reports incomplete output without a reason this adapter can express",
            )),
        },
        "failed" => Ok(FinishReason::Error),
        _ => Err(malformed(
            "the document is not a finished answer (its `status` is not `completed`, `incomplete` or \
             `failed`)",
        )),
    }
}
