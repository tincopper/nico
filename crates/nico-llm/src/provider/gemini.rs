//! The Gemini provider adapter: canonical values ↔ the `generateContent` wire format.
//!
//! This is one *provider* adapter, not a client codec. It is handed the compiled route facts and a
//! canonical request and answers with an [`Endpoint`] and a body; it reads the provider's event stream
//! back into canonical values. It never learns which client protocol the caller spoke.
//!
//! # What is mapped
//!
//! | Canonical | generateContent |
//! |---|---|
//! | `ChatRequest.model` | the physical model, in the request **path** (see below) |
//! | leading system/developer `Item::Message` | `systemInstruction.parts` (text only) |
//! | user `Item::Message` | one `contents[]` entry with `role: "user"` |
//! | assistant `Item::Message` | one `contents[]` entry with `role: "model"` |
//! | `ContentPart::Text` / `Image` | `parts[].text` / `parts[].inlineData` |
//! | `Item::ToolCall` | a `parts[].functionCall` of the model turn |
//! | `Item::ToolResult` | a `parts[].functionResponse` of a user turn |
//! | `Item::Reasoning` | a `parts[]` entry with `thought: true` and `thoughtSignature` |
//! | `Tool` / `ToolChoice` | `tools[].functionDeclarations[]` / `toolConfig.functionCallingConfig` |
//! | `Sampling` | `generationConfig.{temperature,topP,maxOutputTokens,stopSequences,…}` |
//! | `ResponseFormat` | `generationConfig.responseMimeType` and `responseSchema` |
//! | `ChatResponse` / `StreamEvent` | the event stream of `:streamGenerateContent` |
//!
//! # The model and the operation are route facts
//!
//! This provider puts two values derived from the request into the request line:
//! `/v1beta/models/{model}:streamGenerateContent?alt=sse`. The upstream head is written before the
//! body is decoded, so neither may come from the body. Both are therefore decided at publication:
//!
//! - The route's `model_map` must be **constant** — non-empty and every value the same physical
//!   model. Two aliases for one model are fine; two models are not, because the path can only name
//!   one. A route that names no constant model is refused with
//!   [`EndpointError::RouteFact`](super::EndpointError::RouteFact) and the previous generation keeps
//!   serving.
//! - The unique model is written into the path and recorded on the endpoint
//!   ([`Endpoint::with_model`](super::Endpoint::with_model)), so [`Exchange::prepare`](crate::Exchange::prepare) can refuse a
//!   request that resolved to a different model with `403 model_not_allowed` instead of quietly
//!   answering it with the model the route pinned. An alias therefore does not rename anything here:
//!   `alias → model` must be the model the caller asked for.
//! - The operation is fixed to the streaming one. This adapter's `decode_response` therefore folds a
//!   **stream** into one answer; the shape the caller sees is still decided by the caller's own
//!   `stream` flag (the data plane contract), at the cost of folding a buffered answer instead of
//!   parsing one document. Choosing the operation from the request would make one route serve only
//!   one client shape.
//! - A `passthrough` route is refused ([`EndpointError::RouteFact`](super::EndpointError::RouteFact), `routes[].mode`): forwarding
//!   the caller's bytes to a path fixed at publication would send them to whichever model and
//!   operation this route pinned, which is not the ones the caller asked for.
//!
//! # What the wire cannot express
//!
//! Nothing is dropped silently. A known shape with no field here is a typed refusal:
//!
//! - A remote image has no field: `fileData.fileUri` references a stored file, not an `http` URL, so
//!   [`ProviderError::UnsupportedField`] names `content_part.image.url`. An inline image is
//!   `inlineData` and is written wherever a part may appear; the system instruction is text-only
//!   (an image there names `system_instruction.parts`).
//! - A `ContentPart::File` has no inline upload field; `files` is not declared.
//! - A tool result is a `functionResponse` object, and the canonical result carries only text parts
//!   ([`ProviderError::UnsupportedField`] `tool_result.content` for anything else) and no failure
//!   flag (`tool_result.is_error`).
//! - A `functionResponse` also requires the function's **name**, which the canonical
//!   [`ToolResult`](crate::schema::ToolResult) does not carry: it is resolved from the
//!   `Item::ToolCall` of the same conversation. A result whose call is not in the conversation cannot
//!   be named, and is refused naming `tool_result.id` rather than written with an invented name.
//!
//! # Known gaps
//!
//! - The endpoint is always the streaming operation, so a complete answer is **folded** from the
//!   stream (the `fold` module). The canonical event set has no place for a `thoughtSignature`, so the
//!   reasoning of a folded answer carries no signature; the signature is still written when the
//!   caller replays a `Reasoning` item.
//! - A response that carries no `responseId` is reported under [`DEFAULT_RESPONSE_ID`]. The value is
//!   an identifier, not session content, and inventing one is the only alternative to refusing a
//!   readable answer.
//! - A response chunk that carries no `modelVersion` is refused: the canonical answer names the
//!   physical model that produced it, and a made-up name would be a lie in the caller's answer and in
//!   the `llm.model` log field.
//! - `usageMetadata` is recorded as reported and never normalized (phase 1 keeps the two provenance
//!   buckets apart).
//!
//! # Endpoint
//!
//! The endpoint is the route's `base_url` prefix plus
//! `/v1beta/models/{physical}:streamGenerateContent?alt=sse`, with `content-type`,
//! `accept: text/event-stream` and, when the route presents one, `x-goog-api-key`. The URL names an
//! API root or a prefix placed in front of the *whole* operation path, so a base URL that ends in the
//! revision segment (`…/v1beta`) repeats it: write the root, or a proxy prefix of your own. The data plane
//! owns `host`, `content-length`, `transfer-encoding` and every hop-by-hop header. [`VERSION`] is the
//! API revision in the path and the value [`Provider::version`] reports.
//!
//! `providers[].max_output_tokens` is not required here and is not used as a default: this dialect
//! allows `maxOutputTokens` to be omitted, so a request that names no ceiling simply omits the field
//! (the same rule the OpenAI dialects follow). The caller's own ceiling always wins.
//!
//! # Failures
//!
//! A non-2xx response becomes [`ProviderError::Upstream`] carrying the canonical error built by the
//! status table in this adapter's `error` module; the provider's `error.status` is kept only as a
//! bounded log token, and its message, body and headers never reach the caller. A 2xx body this
//! adapter cannot read is [`ProviderError::Malformed`], whose diagnostic names a field or a parse
//! position, never the payload.
//!
//! # Capabilities
//!
//! Streaming, tools, images, reasoning and a structured response format are declared because each is
//! mapped in both directions. `files` is not, for the reason above.

use std::collections::BTreeMap;

use super::{Endpoint, Provider, ProviderError, ProviderKind, ProviderRoute, ProviderStream};
use crate::{
    protocol::{Capabilities, Capability, ProtocolId, ResponseFacts},
    schema::{CanonicalError, ChatRequest, ChatResponse, RawJson, SchemaError},
    wire::json,
};

mod error;
mod fold;
mod parts;
mod request;
mod response;
mod stream;
mod url;
mod usage;

/// The provider kind this adapter registers under.
pub const PROVIDER_KIND: ProviderKind = ProviderKind::new("gemini");

/// The wire protocol this provider speaks natively.
///
/// The value is the client codec's protocol identifier as well, because a `passthrough` route is
/// legal only when both sides name the same protocol — and a `passthrough` route of this kind is
/// refused outright, for the reason the module documentation records.
pub const NATIVE_PROTOCOL: ProtocolId = ProtocolId::new("gemini.generate_content");

/// The API revision this adapter is pinned to.
///
/// It is the `v1beta` segment of the request path and the value [`Provider::version`] reports; one
/// constant, so the path and the assembly log cannot name different revisions.
pub const VERSION: &str = "v1beta";

/// The response id reported when the answer names none.
///
/// The canonical [`ChatResponse`] requires an identifier and this dialect reports one only sometimes.
/// The value is a label a client may log, not session content, so the stable constant is preferable
/// to refusing an otherwise complete answer; see the module's known gaps.
pub const DEFAULT_RESPONSE_ID: &str = "gemini-response";

/// The prefix of the call id this dialect assigns when a call names none.
///
/// One dialect, one spelling: the rule lives beside the client codec's protocol constants, and both
/// sides assign under it.
pub use crate::protocol::gemini::CALL_ID_PREFIX;

/// The adapter for Gemini's `generateContent` wire format.
///
/// The value holds configuration only and no per-request state, so one instance serves every route
/// of this kind and a stream's partial frame lives in the decoder created for that response.
#[derive(Debug, Clone, Copy, Default)]
pub struct GeminiProvider;

impl GeminiProvider {
    /// Builds the adapter.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Provider for GeminiProvider {
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
/// The capability set names features, not positions: an image is expressible in a conversation turn
/// and not in the system instruction, and no capability axis says "images here".
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
