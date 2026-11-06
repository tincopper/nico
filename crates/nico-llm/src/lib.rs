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
//!   compiled route facts an adapter reads.
//! - [`filter`] — [`Exchange`], the one place a codec and an adapter meet.
//!
//! The shared SSE framing lives in `stream/` (crate-private: it is a framing detail, not a contract),
//! and the OpenAI Chat Completions codec lives in [`protocol::openai_chat`]. Vendor names belong only
//! in those modules; the canonical model, the two contracts and the exchange never learn them.
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

pub mod filter;
pub mod protocol;
pub mod provider;
pub mod schema;

mod stream;

pub use filter::{Exchange, ExchangeBuildError, ExchangeError, PreparedExchange, StreamEncoder};
pub use protocol::{
    Capabilities, Capability, ClientCodec, ClientReply, ClientStreamEncoder, CodecError,
    ProtocolId, ProtocolRegistry, RegistryError, RequestFacts, ResponseFacts,
};
pub use provider::{
    Credential, Endpoint, EndpointError, ExchangeMode, Provider, ProviderError, ProviderKind,
    ProviderRegistry, ProviderRoute, ProviderStream, RouteError, WireRequest,
};
pub use schema::{
    AssistantTurn, CanonicalError, ChatRequest, ChatResponse, Choice, ContentPart, Extensions,
    FinishReason, ImageSource, Item, JsonSchema, Message, ModelId, ModelRef, RawJson, Reasoning,
    ResponseFormat, ResponseId, Role, Sampling, SchemaError, StreamEvent, TokenKind, Tool,
    ToolCall, ToolCallId, ToolChoice, ToolName, ToolResult, Usage, UsageCounts,
};
