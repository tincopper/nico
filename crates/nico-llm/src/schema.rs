//! The canonical LLM value model.
//!
//! The shapes here are deliberately not any vendor's wire format. A conversation is a list of
//! [`Item`] values (message, tool call, tool result, reasoning) rather than one vendor's `messages[]`
//! array; a stream is a sequence of [`StreamEvent`] deltas rather than one vendor's chunk object;
//! and the three top-level shapes ([`ChatRequest`], [`ChatResponse`], [`StreamEvent`]) keep their
//! unmodelled fields in [`Extensions`] instead of dropping them.
//!
//! Modules are split by data structure, not by technical layer: one file owns one shape (or one
//! closely related pair) together with its constructors, accessors and validation.
//!
//! # Validation
//!
//! Construction is the trust boundary. `try_new` and the `with_*` methods validate what they set
//! and return [`SchemaError`] naming the field that failed; the one check that needs the whole shape
//! — an extension key shadowing a modelled field — is left to `validate`. `Serialize`/`Deserialize`
//! exist for golden fixtures and diagnostics, not as a second trust boundary. A leaf type whose
//! invariant is a property of its own field ([`ModelId`], [`Sampling`], [`JsonSchema`],
//! [`CanonicalError`]) validates while deserializing; every other shape is validated by its
//! enclosing tree ([`ChatRequest::validate`] / [`ChatResponse::validate`]), which must run before a
//! value is used on a request path.
//!
//! # The one untyped value
//!
//! [`RawJson`] wraps `serde_json::Value`. It covers the JSON a caller declares — tool parameters,
//! tool-call arguments and a structured-output schema, whose shape the gateway cannot know — and the
//! unmodelled fields inside [`Extensions`]. Configuration, routing and plugin data never use it;
//! those paths stay typed.
//!
//! The serde form captures an unknown *top-level* key in [`Extensions`] and ignores an unknown key
//! inside a nested shape. Nested unknowns are the codec's job, because only the codec knows its
//! wire spelling: it lifts such a field into the enclosing extensions or rejects the request as
//! unsupported, and never drops it silently.

mod conversation;
mod error;
mod extension;
mod model;
mod request;
mod response;
mod stream;
mod tool;
mod usage;

pub use conversation::{ContentPart, ImageSource, Item, Message, Reasoning, Role};
pub use error::{CanonicalError, SchemaError};
pub use extension::{Extensions, RawJson};
pub use model::{ModelId, ModelRef};
pub use request::{ChatRequest, JsonSchema, ResponseFormat, Sampling};
pub use response::{AssistantTurn, ChatResponse, Choice, FinishReason, ResponseId};
pub use stream::StreamEvent;
pub use tool::{Tool, ToolCall, ToolCallId, ToolChoice, ToolName, ToolResult};
pub use usage::{TokenKind, Usage, UsageCounts};
