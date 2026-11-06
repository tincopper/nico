//! Canonical streaming events.
//!
//! [`StreamEvent`] is not any vendor's chunk. Every target protocol streams differently — a delta
//! array on a choice, a content-block delta, an output-text event, a candidate part — and a model
//! that adopts one of them as the intermediate form ends up reverse-engineering the others. These
//! events are the small common vocabulary: a start marker, incremental text/reasoning/tool-argument
//! deltas, usage, a finish reason, and an error.
//!
//! Each response gets its own decoder instance; nothing in this enum carries decoder state. The
//! SSE framing that produces chunk boundaries lives outside `schema/`.

use serde::{Deserialize, Serialize};

use super::{CanonicalError, FinishReason, ModelId, ResponseId, ToolCallId, ToolName, Usage};

/// One increment of a streamed model response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
pub enum StreamEvent {
    /// The response began; carries the provider response id and physical model.
    Start {
        /// Provider response id, also used by later events.
        id: ResponseId,
        /// Physical model the provider is running.
        model: ModelId,
    },
    /// A piece of visible text.
    TextDelta(String),
    /// A piece of hidden reasoning.
    ReasoningDelta(String),
    /// A tool call began; the arguments arrive in [`StreamEvent::ToolCallDelta`] fragments.
    ToolCallStart {
        /// The call being built.
        id: ToolCallId,
        /// The tool being called.
        name: ToolName,
    },
    /// A fragment of a tool call's JSON arguments.
    ///
    /// The fragments are not individually valid JSON; concatenating them in order yields the
    /// arguments document.
    ToolCallDelta {
        /// The call the fragment belongs to.
        id: ToolCallId,
        /// A text fragment of the JSON arguments.
        arguments: String,
    },
    /// A tool call's arguments are complete.
    ToolCallEnd {
        /// The completed call.
        id: ToolCallId,
    },
    /// Token usage for the response so far.
    Usage(Usage),
    /// Generation stopped; no further content events follow.
    Finish(FinishReason),
    /// The stream failed after it started. No synthetic finish event follows.
    Error(CanonicalError),
}
