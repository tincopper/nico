//! The state machine behind [`super::Decoder`]: one response's blocks, identity and completeness.
//!
//! The value is created per response and holds everything that outlives one event: the framing's
//! partial frame, the identity `message_start` announced, the blocks still open, and the flags that
//! make a truncated or failed stream distinguishable from a complete one. The wire documents it reads
//! are declared by the parent module; the refusals come from the adapter's helpers.

use std::collections::{BTreeMap, BTreeSet};

use super::{
    super::{MAX_CONTENT_BLOCKS, error, finish_reason, malformed, parse, reject_extra, usage},
    BlockStartWire, DeltaWire, EventWire, MessageWire, sse_failure,
};
use crate::{
    provider::{ProviderError, ProviderStream},
    schema::{ModelId, ResponseId, StreamEvent, ToolCallId, ToolName},
    stream::sse,
};

/// The decoder for one streamed response.
pub(in crate::provider::anthropic) struct Decoder {
    events: sse::Decoder,
    identity: Option<(ResponseId, ModelId)>,
    blocks: BTreeMap<u32, OpenBlock>,
    closed: BTreeSet<u32>,
    finished: bool,
    done: bool,
    failed: bool,
}

impl Decoder {
    /// Creates a decoder that has not seen the response's first event yet.
    pub(in crate::provider::anthropic) fn new() -> Self {
        Self {
            events: sse::Decoder::new(),
            identity: None,
            blocks: BTreeMap::new(),
            closed: BTreeSet::new(),
            finished: false,
            done: false,
            failed: false,
        }
    }

    /// Turns the events one chunk of bytes completed into canonical events.
    fn absorb(&mut self, events: Vec<sse::Event>) -> Result<Vec<StreamEvent>, ProviderError> {
        let mut out = Vec::new();
        for event in events {
            if self.done || self.failed {
                break;
            }
            // A heartbeat carries no payload; an event with nothing but whitespace says nothing.
            if event.data.trim().is_empty() {
                continue;
            }
            let wire: EventWire = parse(event.data.as_bytes(), "a stream event")?;
            self.event(wire, &mut out)?;
        }
        Ok(out)
    }

    /// Dispatches one event by its `type` member.
    ///
    /// The `event:` line of the framing repeats the same value, but the payload is the document the
    /// decoder reads: a frame whose name and payload disagreed would otherwise have two meanings. A
    /// type this adapter does not know is skipped, and only the ones that carry content or the end of
    /// the answer are dispatched.
    fn event(
        &mut self,
        mut wire: EventWire,
        out: &mut Vec<StreamEvent>,
    ) -> Result<(), ProviderError> {
        let kind = std::mem::take(&mut wire.kind);
        match kind.as_str() {
            "message_start" => self.start(wire, out),
            "content_block_start" => self.block_start(wire, out),
            "content_block_delta" => self.block_delta(wire, out),
            "content_block_stop" => self.block_stop(wire, out),
            "message_delta" => self.message_delta(wire, out),
            "message_stop" => {
                self.done = true;
                Ok(())
            }
            "error" => {
                out.push(StreamEvent::Error(error::stream_failure(
                    wire.error.as_ref().and_then(|body| body.kind.as_ref()),
                )));
                self.failed = true;
                Ok(())
            }
            // `ping` is a heartbeat and any other type is not part of this answer.
            _ => Ok(()),
        }
    }

    /// Emits the start event, and refuses a stream that announces its response twice.
    fn start(&mut self, wire: EventWire, out: &mut Vec<StreamEvent>) -> Result<(), ProviderError> {
        if self.identity.is_some() {
            return Err(malformed(
                "the stream announced its response more than once",
            ));
        }
        let MessageWire {
            id,
            model,
            kind,
            role,
            usage,
            _extra,
        } = wire
            .message
            .ok_or_else(|| malformed("the stream's first event carries no message"))?;
        if let Some(kind) = &kind
            && kind != "message"
        {
            return Err(malformed(
                "the stream announced something that is not a message",
            ));
        }
        if let Some(role) = &role
            && role != "assistant"
        {
            return Err(malformed("the stream announced a non-assistant message"));
        }
        let id = ResponseId::try_new(id).map_err(super::super::schema_failure)?;
        let model = ModelId::try_new(model).map_err(super::super::schema_failure)?;
        self.identity = Some((id.clone(), model.clone()));
        out.push(StreamEvent::Start { id, model });
        if let Some(usage) = &usage {
            out.push(StreamEvent::Usage(usage::decode(usage)));
        }
        Ok(())
    }

    /// Opens one content block.
    fn block_start(
        &mut self,
        wire: EventWire,
        out: &mut Vec<StreamEvent>,
    ) -> Result<(), ProviderError> {
        self.ready()?;
        if self.finished {
            return Err(malformed(
                "the stream opened a content block after its finish reason",
            ));
        }
        let index = self.index(wire.index)?;
        let block = wire
            .content_block
            .ok_or_else(|| malformed("a content block start carries no block"))?;
        let BlockStartWire {
            kind,
            text,
            thinking,
            signature,
            id,
            name,
            input,
            extra,
        } = block;
        reject_extra(&extra, "content_block")?;
        if index >= MAX_CONTENT_BLOCKS {
            return Err(malformed(
                "the stream opened more content blocks than this build serves",
            ));
        }
        if self.closed.contains(&index) || self.blocks.contains_key(&index) {
            return Err(malformed("the stream reused a content block index"));
        }
        match kind.as_str() {
            "text" => {
                self.blocks.insert(index, OpenBlock::Text);
                if let Some(text) = text.filter(|text| !text.is_empty()) {
                    out.push(StreamEvent::TextDelta(text));
                }
            }
            "thinking" => {
                // A signature delivered with the block start is the value a `signature_delta` would
                // carry; it has no canonical event either (see the parent module's document), so it
                // is read and dropped rather than refused.
                let _ = signature;
                self.blocks.insert(index, OpenBlock::Thinking);
                if let Some(text) = thinking.filter(|text| !text.is_empty()) {
                    out.push(StreamEvent::ReasoningDelta(text));
                }
            }
            "tool_use" => {
                let id = id.ok_or_else(|| malformed("a streamed tool call carries no id"))?;
                let name = name.ok_or_else(|| malformed("a streamed tool call carries no name"))?;
                // The arguments arrive as `input_json_delta` fragments; this protocol starts the
                // block with the empty object, and a non-empty one would be arguments outside a
                // delta that the event stream cannot order.
                if let Some(input) = &input
                    && !input.as_value().is_null()
                    && !input.is_empty_object()
                {
                    return Err(malformed(
                        "a streamed tool call started with arguments outside a delta",
                    ));
                }
                let id = ToolCallId::try_new(id).map_err(super::super::schema_failure)?;
                let name = ToolName::try_new(name).map_err(super::super::schema_failure)?;
                // The name arrives once, at the start; later fragments address the call by index and
                // id only, so the decoder keeps the id it must repeat and not the name.
                self.blocks
                    .insert(index, OpenBlock::Tool { id: id.clone() });
                out.push(StreamEvent::ToolCallStart { id, name, index });
            }
            _ => {
                return Err(malformed(
                    "the stream opened a content block this adapter cannot express",
                ));
            }
        }
        Ok(())
    }

    /// Decodes one increment of an open block.
    fn block_delta(
        &mut self,
        wire: EventWire,
        out: &mut Vec<StreamEvent>,
    ) -> Result<(), ProviderError> {
        self.ready()?;
        if self.finished {
            return Err(malformed(
                "the stream carried a content delta after its finish reason",
            ));
        }
        let index = self.index(wire.index)?;
        let delta = wire
            .delta
            .ok_or_else(|| malformed("a content block delta carries no delta"))?;
        let DeltaWire {
            kind,
            text,
            partial_json,
            thinking,
            signature,
            stop_reason: _,
            _stop_sequence,
            extra,
        } = delta;
        reject_extra(&extra, "delta")?;
        let kind = kind.ok_or_else(|| malformed("a content block delta names no kind"))?;
        let Some(open) = self.blocks.get(&index) else {
            return Err(malformed("a content delta names no open content block"));
        };
        match (open, kind.as_str()) {
            (OpenBlock::Text, "text_delta") => {
                if let Some(text) = text.filter(|text| !text.is_empty()) {
                    out.push(StreamEvent::TextDelta(text));
                }
            }
            (OpenBlock::Thinking, "thinking_delta") => {
                if let Some(text) = thinking.filter(|text| !text.is_empty()) {
                    out.push(StreamEvent::ReasoningDelta(text));
                }
            }
            (OpenBlock::Tool { id }, "input_json_delta") => {
                if let Some(arguments) = partial_json.filter(|text| !text.is_empty()) {
                    out.push(StreamEvent::ToolCallDelta {
                        id: id.clone(),
                        arguments,
                        index,
                    });
                }
            }
            // The signature closes a thinking block. It is part of the document, so its absence is
            // refused, and it has no canonical event — see the parent module's document.
            (OpenBlock::Thinking, "signature_delta") => {
                if signature.is_none() {
                    return Err(malformed("a signature delta carries no signature"));
                }
            }
            _ => {
                return Err(malformed(
                    "a content delta does not match the block it belongs to",
                ));
            }
        }
        Ok(())
    }

    /// Closes one content block, ending a tool call when the block was one.
    fn block_stop(
        &mut self,
        wire: EventWire,
        out: &mut Vec<StreamEvent>,
    ) -> Result<(), ProviderError> {
        self.ready()?;
        let index = self.index(wire.index)?;
        let Some(open) = self.blocks.remove(&index) else {
            return Err(malformed("a content block stopped without having started"));
        };
        self.closed.insert(index);
        if let OpenBlock::Tool { id } = open {
            out.push(StreamEvent::ToolCallEnd { id, index });
        }
        Ok(())
    }

    /// Reads the usage and the finish reason one `message_delta` carries.
    fn message_delta(
        &mut self,
        wire: EventWire,
        out: &mut Vec<StreamEvent>,
    ) -> Result<(), ProviderError> {
        self.ready()?;
        let delta = wire
            .delta
            .ok_or_else(|| malformed("a message delta carries no delta"))?;
        reject_extra(&delta.extra, "delta")?;
        // Usage is written before the finish event: the caller reads the answer's accounting before
        // its end, and a later `message_delta` may report usage after the reason without reopening
        // the answer.
        if let Some(usage) = &wire.usage {
            out.push(StreamEvent::Usage(usage::decode(usage)));
        }
        let Some(reason) = delta.stop_reason else {
            return Ok(());
        };
        if self.finished {
            return Err(malformed(
                "the stream reported a finish reason more than once",
            ));
        }
        if !self.blocks.is_empty() {
            return Err(malformed(
                "the stream finished with a content block still open",
            ));
        }
        out.push(StreamEvent::Finish(finish_reason(&reason)?));
        self.finished = true;
        Ok(())
    }

    /// Returns the peer-chosen index of a block event.
    fn index(&self, index: Option<u32>) -> Result<u32, ProviderError> {
        index.ok_or_else(|| malformed("a content block event names no index"))
    }

    /// Refuses a content event that precedes the response it belongs to.
    fn ready(&self) -> Result<(), ProviderError> {
        if self.identity.is_none() {
            return Err(malformed(
                "a stream event arrived before the response it belongs to",
            ));
        }
        Ok(())
    }
}

impl ProviderStream for Decoder {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<StreamEvent>, ProviderError> {
        if self.done || self.failed {
            return Ok(Vec::new());
        }
        let events = self.events.push(chunk).map_err(sse_failure)?;
        self.absorb(events)
    }

    fn finish(&mut self) -> Result<Vec<StreamEvent>, ProviderError> {
        if self.failed {
            return Ok(Vec::new());
        }
        if self.done && self.finished {
            return Ok(Vec::new());
        }
        let events = self.events.finish().map_err(sse_failure)?;
        let out = self.absorb(events)?;
        if self.failed {
            return Ok(out);
        }
        // A body that ended before a finish reason cannot be reported as a finished answer, whether
        // or not the provider sent its terminator.
        if !self.finished {
            return Err(malformed("the stream ended without a finish reason"));
        }
        Ok(out)
    }
}

/// One content block still being assembled.
enum OpenBlock {
    Text,
    Thinking,
    Tool { id: ToolCallId },
}
