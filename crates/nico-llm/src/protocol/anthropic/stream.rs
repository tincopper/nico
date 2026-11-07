//! Encoding canonical stream events as Messages stream events.
//!
//! This protocol's stream is a sequence of *named* events, and its content arrives inside numbered
//! content blocks: text, thinking and each tool call occupy a block, and a delta only makes sense
//! after the block that owns its index was opened. The encoder therefore keeps three facts for one
//! response — the id and model the start event announced, the currently open block, and which wire
//! block each canonical tool call occupies. That state is why [`super::AnthropicMessagesCodec`] stays
//! stateless while a stream still carries what a caller reads from every event.
//!
//! # Ordering rules
//!
//! - A block-type change closes the open block first, so the caller never sees a delta for an index
//!   that has already been stopped.
//! - [`StreamEvent::Finish`] closes the open block and writes the `message_delta` that announces the
//!   stop reason. A [`StreamEvent::Usage`] that arrives *after* it writes a second `message_delta`,
//!   because a provider that reports usage late must still land before the terminator.
//! - The terminator is `message_stop`, written when the response body ends rather than when the finish
//!   event arrives: the usage case above is exactly why.
//! - A failing stream ([`StreamEvent::Error`]) writes the protocol's `error` event and stops; no
//!   terminator follows, because the body ending is how a caller learns the answer was cut short.

use std::collections::BTreeMap;

use serde::Serialize;

use super::{PROTOCOL, error, finish_reason, response::MESSAGE_OBJECT, usage};
use crate::{
    protocol::{ClientStreamEncoder, CodecError},
    schema::{RawJson, StreamEvent},
};

/// The block currently open in the stream, with the index the caller reads.
#[derive(Debug, Clone, Copy)]
enum Open {
    Text(u32),
    Thinking(u32),
    Tool(u32),
}

impl Open {
    /// Returns the wire index of the open block.
    const fn index(self) -> u32 {
        match self {
            Self::Text(index) | Self::Thinking(index) | Self::Tool(index) => index,
        }
    }
}

/// The encoder for one response stream.
#[derive(Debug, Default)]
pub(super) struct Encoder {
    id: Option<String>,
    open: Option<Open>,
    next_index: u32,
    /// The wire block index of each canonical tool call, keyed by the call's position.
    calls: BTreeMap<u32, u32>,
    usage: Option<usage::UsageWire>,
    stop_reason: Option<&'static str>,
    finished: bool,
    terminated: bool,
}

impl Encoder {
    /// Creates an encoder that has not seen the stream's start event yet.
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Writes one named event whose payload is already serialized.
    fn event<T: Serialize>(&self, name: &str, payload: &T) -> Result<Vec<u8>, CodecError> {
        let document = serde_json::to_string(payload).map_err(|failure| CodecError::Encode {
            protocol: PROTOCOL,
            reason: failure.to_string(),
        })?;
        let mut bytes = Vec::with_capacity(document.len() + name.len() + 12);
        bytes.extend_from_slice(b"event: ");
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(b"\ndata: ");
        bytes.extend_from_slice(document.as_bytes());
        bytes.extend_from_slice(b"\n\n");
        Ok(bytes)
    }

    /// Closes the open block, if there is one.
    fn close(&mut self) -> Result<Vec<u8>, CodecError> {
        let Some(open) = self.open.take() else {
            return Ok(Vec::new());
        };
        self.event(
            "content_block_stop",
            &StopEvent {
                kind: "content_block_stop",
                index: open.index(),
            },
        )
    }

    /// Opens a new block, closing the previous one first.
    ///
    /// The index counter only grows: a closed block's index is never reused, because a caller keys
    /// its rendering on it.
    fn open(
        &mut self,
        block: BlockStart<'_>,
        kind: fn(u32) -> Open,
    ) -> Result<(u32, Vec<u8>), CodecError> {
        let index = self.next_index;
        self.next_index = self.next_index.saturating_add(1);
        self.open = Some(kind(index));
        let bytes = self.event(
            "content_block_start",
            &StartEvent {
                kind: "content_block_start",
                index,
                content_block: block,
            },
        )?;
        Ok((index, bytes))
    }

    /// Emits one delta for the block at `index`.
    fn delta(&self, index: u32, delta: Delta<'_>) -> Result<Vec<u8>, CodecError> {
        self.event(
            "content_block_delta",
            &DeltaEvent {
                kind: "content_block_delta",
                index,
                delta,
            },
        )
    }

    /// Returns the open text block, opening one when the current block is something else.
    fn text_block(&mut self) -> Result<(u32, Vec<u8>), CodecError> {
        if let Some(Open::Text(index)) = self.open {
            return Ok((index, Vec::new()));
        }
        let mut bytes = self.close()?;
        let (index, opened) = self.open(BlockStart::Text { text: "" }, Open::Text)?;
        bytes.extend(opened);
        Ok((index, bytes))
    }

    /// Returns the open reasoning block, opening one when the current block is something else.
    fn thinking_block(&mut self) -> Result<(u32, Vec<u8>), CodecError> {
        if let Some(Open::Thinking(index)) = self.open {
            return Ok((index, Vec::new()));
        }
        let mut bytes = self.close()?;
        let (index, opened) = self.open(BlockStart::Thinking { thinking: "" }, Open::Thinking)?;
        bytes.extend(opened);
        Ok((index, bytes))
    }

    /// Writes the `message_delta` that carries a stop reason and, when known, usage.
    fn message_delta(
        &mut self,
        reason: &'static str,
        usage: Option<usage::UsageWire>,
    ) -> Result<Vec<u8>, CodecError> {
        self.stop_reason = Some(reason);
        self.finished = true;
        self.event(
            "message_delta",
            &MessageDeltaEvent {
                kind: "message_delta",
                delta: MessageDeltaBody {
                    stop_reason: reason,
                    stop_sequence: None,
                },
                usage,
            },
        )
    }

    /// Refuses a content event that arrived before the start event.
    fn require_start(&self) -> Result<(), CodecError> {
        if self.id.is_some() {
            return Ok(());
        }
        Err(CodecError::Encode {
            protocol: PROTOCOL,
            reason: "a stream event arrived before the start event".to_owned(),
        })
    }
}

impl ClientStreamEncoder for Encoder {
    fn push(&mut self, event: &StreamEvent) -> Result<Vec<u8>, CodecError> {
        if self.terminated || (self.finished && !matches!(event, StreamEvent::Usage(_))) {
            return Ok(Vec::new());
        }
        match event {
            StreamEvent::Start { id, model } => {
                self.id = Some(id.as_str().to_owned());
                self.event(
                    "message_start",
                    &MessageStartEvent {
                        kind: "message_start",
                        message: MessageBody {
                            id: id.as_str(),
                            kind: MESSAGE_OBJECT,
                            role: "assistant",
                            model: model.as_str(),
                            content: Vec::new(),
                            stop_reason: None,
                            stop_sequence: None,
                            usage: usage::UsageWire::default(),
                        },
                    },
                )
            }
            StreamEvent::TextDelta(text) => {
                self.require_start()?;
                let (index, mut bytes) = self.text_block()?;
                bytes.extend(self.delta(index, Delta::Text { text })?);
                Ok(bytes)
            }
            StreamEvent::ReasoningDelta(text) => {
                self.require_start()?;
                let (index, mut bytes) = self.thinking_block()?;
                bytes.extend(self.delta(index, Delta::Thinking { thinking: text })?);
                Ok(bytes)
            }
            StreamEvent::ToolCallStart { id, name, index } => {
                self.require_start()?;
                let mut bytes = self.close()?;
                let (block, opened) = self.open(
                    BlockStart::ToolUse {
                        id: id.as_str(),
                        name: name.as_str(),
                        input: RawJson::new(serde_json::json!({})),
                    },
                    Open::Tool,
                )?;
                bytes.extend(opened);
                self.calls.insert(*index, block);
                Ok(bytes)
            }
            StreamEvent::ToolCallDelta {
                arguments, index, ..
            } => {
                self.require_start()?;
                let block = self
                    .calls
                    .get(index)
                    .copied()
                    .ok_or_else(|| CodecError::Encode {
                        protocol: PROTOCOL,
                        reason: "a tool-call fragment arrived before its block started".to_owned(),
                    })?;
                self.delta(
                    block,
                    Delta::InputJson {
                        partial_json: arguments,
                    },
                )
            }
            StreamEvent::ToolCallEnd { index, .. } => {
                if matches!(self.open, Some(Open::Tool(open)) if Some(&open) == self.calls.get(index))
                {
                    return self.close();
                }
                Ok(Vec::new())
            }
            StreamEvent::Usage(report) => {
                let encoded = usage::encode(report);
                let Some(reason) = self.stop_reason else {
                    // Before the finish event the caller already has a report? No: this event is the
                    // report, and the protocol carries it on the delta that announces the stop
                    // reason, so it is held until that delta exists.
                    self.usage = Some(encoded);
                    return Ok(Vec::new());
                };
                // A report that arrived after the finish event: a second `message_delta` keeps it
                // before the terminator, which has not been written yet.
                self.message_delta(reason, Some(encoded))
            }
            StreamEvent::Finish(reason) => {
                let mut bytes = self.close()?;
                let usage = self.usage.take();
                bytes.extend(self.message_delta(finish_reason(*reason), usage)?);
                Ok(bytes)
            }
            StreamEvent::Error(failure) => {
                self.terminated = true;
                error::frame(failure)
            }
        }
    }

    fn finish(&mut self) -> Result<Vec<u8>, CodecError> {
        if self.terminated {
            return Ok(Vec::new());
        }
        self.terminated = true;
        if self.id.is_none() {
            return Ok(Vec::new());
        }
        let mut bytes = self.close()?;
        bytes.extend(self.event(
            "message_stop",
            &TypeOnly {
                kind: "message_stop",
            },
        )?);
        Ok(bytes)
    }
}

/// The `message_start` event.
#[derive(Serialize)]
struct MessageStartEvent<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    message: MessageBody<'a>,
}

/// The message a stream opens with: empty content and no stop reason yet.
#[derive(Serialize)]
struct MessageBody<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    role: &'static str,
    model: &'a str,
    content: Vec<BlockStart<'a>>,
    stop_reason: Option<&'static str>,
    /// Always present and always `null`: the canonical model does not record which sequence matched.
    stop_sequence: Option<&'a str>,
    usage: usage::UsageWire,
}

/// The `content_block_start` event.
#[derive(Serialize)]
struct StartEvent<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    index: u32,
    content_block: BlockStart<'a>,
}

/// A block a stream opens.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BlockStart<'a> {
    /// A text block; the deltas carry what it says.
    Text {
        /// Empty at open time.
        text: &'a str,
    },
    /// A reasoning block; the deltas carry what it thinks.
    Thinking {
        /// Empty at open time.
        thinking: &'a str,
    },
    /// A tool call; the argument fragments arrive as deltas.
    ToolUse {
        /// The call's identifier.
        id: &'a str,
        /// The tool being called.
        name: &'a str,
        /// The arguments object, which the deltas fill in.
        input: RawJson,
    },
}

/// The `content_block_delta` event.
#[derive(Serialize)]
struct DeltaEvent<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    index: u32,
    delta: Delta<'a>,
}

/// One increment of a block.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Delta<'a> {
    /// A piece of visible text.
    #[serde(rename = "text_delta")]
    Text {
        /// The text fragment.
        text: &'a str,
    },
    /// A piece of hidden reasoning.
    #[serde(rename = "thinking_delta")]
    Thinking {
        /// The reasoning fragment.
        thinking: &'a str,
    },
    /// A fragment of a tool call's JSON arguments.
    #[serde(rename = "input_json_delta")]
    InputJson {
        /// The fragment; concatenating the fragments yields the arguments document.
        partial_json: &'a str,
    },
}

/// The `content_block_stop` event.
#[derive(Serialize)]
struct StopEvent {
    #[serde(rename = "type")]
    kind: &'static str,
    index: u32,
}

/// The `message_delta` event.
#[derive(Serialize)]
struct MessageDeltaEvent {
    #[serde(rename = "type")]
    kind: &'static str,
    delta: MessageDeltaBody,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<usage::UsageWire>,
}

/// What ended the message.
#[derive(Serialize)]
struct MessageDeltaBody {
    stop_reason: &'static str,
    stop_sequence: Option<&'static str>,
}

/// An event whose only member is its own name.
#[derive(Serialize)]
struct TypeOnly {
    #[serde(rename = "type")]
    kind: &'static str,
}
