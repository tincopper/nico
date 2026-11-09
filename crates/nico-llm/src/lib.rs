//! Canonical LLM model shared by client protocol codecs and provider adapters.
//!
//! A gateway sits between a client protocol (OpenAI Chat, OpenAI Responses, Anthropic Messages,
//! Gemini) and a provider that may speak a different one. Those are two independent axes; this
//! crate's [`schema`] is the only vocabulary both sides share. A client codec decodes its wire
//! format into canonical values and encodes canonical values back; a provider adapter does the same
//! against the provider's wire format. Neither side names the other, so the number of translation
//! paths stays *clients + providers* instead of *clients × providers*.
//!
//! # Modules
//!
//! - [`schema`] — the canonical request, response, stream event, usage and error shapes. It depends
//!   on `serde` and plain data only: no protocol, provider or data-plane type crosses into it.
//! - [`protocol`] — the [`ClientCodec`] contract, its registry, the [`Capabilities`] a protocol
//!   declares, and the framework-free facts of one HTTP exchange.
//! - [`provider`] — the [`Provider`] and [`ProviderStream`] contracts, their registry, and the
//!   compiled route facts an adapter reads. One module per wire dialect: [`provider::openai`] speaks
//!   Chat Completions, [`provider::openai_responses`] speaks Responses (the same vendor's second
//!   dialect is a second kind, because a kind names one document shape), and
//!   [`provider::anthropic`] speaks Messages, and [`provider::gemini`] speaks `generateContent`.
//! - [`filter`] — [`Exchange`], the one place a codec and an adapter meet.
//! - [`route`] — the front-ends the data plane publishes: one converting route (exchange plus a
//!   per-response transform) and one passthrough route (endpoint only).
//!
//! The shared SSE framing lives in `stream/` (crate-private: it is a framing detail, not a contract),
//! the shared wire helpers live in `wire/` (crate-private: bytes and JSON, no protocol semantics), the
//! shared diagnostic rules live in `diagnostic.rs` (crate-private: what a refusal may quote), the
//! OpenAI Chat Completions codec lives in [`protocol::openai_chat`], and the provider adapters live in
//! [`provider::openai`], [`provider::openai_responses`], [`provider::anthropic`] and
//! [`provider::gemini`]. Vendor names
//! belong only in those modules; the
//! canonical model, the two contracts and the exchange never learn them. [`builtin_registry`] is the
//! one place the kinds this build serves are listed, so the process assembles a registry rather than a
//! vendor table.
//!
//! # Invariants
//!
//! - **A request is a value.** Convert-mode routes buffer the client body once and decode it into a
//!   [`ChatRequest`]; a retry or a model change re-encodes that value rather than replaying bytes.
//! - **Unknown fields are preserved.** [`Extensions`] carries what the model does not name, and
//!   [`RawJson`] is the only untyped value in the model. A codec may reject a field it cannot
//!   express, but it never drops one silently.
//! - **Constructors validate.** Public constructors return [`SchemaError`] naming the field that
//!   failed. The serde representation exists for golden fixtures and diagnostics: a value read
//!   through serde must pass the enclosing `validate` before it is used on a request path, which is
//!   why [`Exchange`] validates what a codec or adapter produced.
//! - **The exchange fails as a value.** Every failure leaves as an [`ExchangeError`], which renders
//!   itself as a canonical error; the client codec owns the client's error envelope, so a provider's
//!   body and credential never reach the caller.

#![deny(missing_docs)]

use std::sync::Arc;

pub mod filter;
pub mod protocol;
pub mod provider;
pub mod route;
pub mod schema;

mod breaker;
mod diagnostic;
mod stream;
mod wire;

pub use breaker::{Admission, AttemptOutcome, BreakerPhase, BreakerTable};
pub use filter::{Exchange, ExchangeBuildError, ExchangeError, PreparedExchange, StreamEncoder};
pub use protocol::{
    Capabilities, Capability, ClientCodec, ClientReply, ClientStreamEncoder, CodecError,
    ProtocolId, ProtocolRegistry, RegistryError, RequestFacts, ResponseFacts,
};
pub use provider::{
    Credential, Endpoint, EndpointError, ExchangeMode, Provider, ProviderError, ProviderKind,
    ProviderRegistry, ProviderRoute, ProviderStream, RouteError, WireRequest,
};
pub use route::{
    AttemptChain, AttemptPlan, AttemptSequence, BreakerParams, ChainBuildError, ChainMember,
    ConvertRoute, PassthroughRoute, SelectionRefused, TargetIdentity, Verdict,
};
pub use schema::{
    AssistantTurn, CanonicalError, ChatRequest, ChatResponse, Choice, ContentPart, Extensions,
    FinishReason, ImageSource, Item, JsonSchema, MAX_CANONICAL_CODE_BYTES,
    MAX_CANONICAL_MESSAGE_BYTES, MAX_TOKEN_BYTES, Message, ModelId, ModelRef, RawJson, Reasoning,
    ResponseFormat, ResponseId, Role, Sampling, SchemaError, StreamEvent, TokenKind, Tool,
    ToolCall, ToolCallId, ToolChoice, ToolName, ToolResult, Usage, UsageCounts,
};

/// Returns the client codecs and provider adapters this build serves.
///
/// The process calls this once while it is assembled and keeps both registries for its lifetime: a
/// codec and an adapter hold configuration and no per-request state, so one value serves every route
/// of its protocol or kind. The pair travels together because publication needs both to compile one
/// route, and a process that took one from here and the other from somewhere else could pair a codec
/// with an adapter nothing assembled.
///
/// A duplicate registration is reported and skipped rather than asserted: the kinds above are
/// distinct, so reaching it means this function was changed incorrectly, and a panic while the
/// process starts is a worse answer than a kind that is registered once. The test that asserts both
/// lookups resolve is what catches it.
#[must_use]
pub fn builtin_registry() -> (ProtocolRegistry, ProviderRegistry) {
    let mut protocols = ProtocolRegistry::new();
    let codecs: [Arc<dyn ClientCodec>; 4] = [
        Arc::new(protocol::openai_chat::OpenAiChatCodec::new()),
        Arc::new(protocol::anthropic::AnthropicMessagesCodec::new()),
        Arc::new(protocol::openai_responses::OpenAiResponsesCodec::new()),
        Arc::new(protocol::gemini::GeminiGenerateContentCodec::new()),
    ];
    for codec in codecs {
        if let Err(error) = protocols.register(codec) {
            tracing::warn!(%error, "a built-in client protocol was registered twice");
        }
    }

    let mut providers = ProviderRegistry::new();
    let adapters: [Arc<dyn Provider>; 4] = [
        Arc::new(provider::openai::OpenAiProvider::new()),
        Arc::new(provider::OpenAiResponsesProvider::new()),
        Arc::new(provider::AnthropicProvider::new()),
        Arc::new(provider::GeminiProvider::new()),
    ];
    for adapter in adapters {
        if let Err(error) = providers.register(adapter) {
            tracing::warn!(%error, "a built-in provider was registered twice");
        }
    }
    (protocols, providers)
}
