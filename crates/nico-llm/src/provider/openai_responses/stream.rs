//! Decoding a Responses event stream into canonical events.
//!
//! One decoder serves one response: it holds the event framing's partial frame, the identity
//! `response.created` announced, the tool calls still being assembled, and whether the stream has
//! finished. A shared decoder would let one response's partial frame be completed by another one's
//! bytes, which is why [`Provider::stream_decoder`](crate::Provider::stream_decoder) creates a fresh
//! value per response.
//!
//! # Event order
//!
//! `response.created` announces the identity, so it emits [`StreamEvent::Start`]. Text and reasoning
//! summary fragments follow as [`StreamEvent::TextDelta`] and [`StreamEvent::ReasoningDelta`]. A tool
//! call arrives in two stages: `response.output_item.added` names it and `output_index`, and the
//! `response.function_call_arguments.*` events carry its arguments. The decoder records the call when
//! it is announced and emits [`StreamEvent::ToolCallStart`] with the first argument fragment, so the
//! canonical `index` is the peer's `output_index` — a client can address a parallel call before its
//! arguments say anything. `response.completed` closes every call that is still open, reports usage,
//! and reports why generation stopped, in that order. Events this adapter does not know are skipped:
//! a stream fragment carries no canonical place for an unknown event.
//!
//! `response.reasoning_summary_text.delta` is rendered, even though a reasoning item can never be
//! sent back (see the adapter's module documentation): the summary is part of the answer the caller
//! receives, and hiding it would hide content.
//!
//! # Truncation is never completeness
//!
//! `response.completed` is this dialect's terminator: it ends the stream, and every byte after it is
//! ignored. A body that ends before it is refused — the client must not be told the model finished
//! when nobody said it did. `[DONE]` is not part of this dialect; a compatible endpoint that sends it
//! is ignored, because the frame adds no fact — the same rule that lets the OpenAI chat decoder accept
//! its absence.
//!
//! A failure is `response.failed` (whose embedded response carries the provider's code) or a bare
//! `error` event. Either becomes [`StreamEvent::Error`] and ends the decoder's output. The provider's
//! message is never read; only a bounded printable `code` is kept for the log, the same value the
//! status table would have kept from a failure body.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::{
    MAX_OUTPUT_ITEMS, malformed, parse,
    response::{OutputItemWire, ResponseWire},
};
use crate::{
    provider::{ProviderError, ProviderStream},
    schema::{ModelId, RawJson, ResponseId, StreamEvent, ToolCallId, ToolName},
    stream::sse,
};

/// The decoder for one streamed response.
pub(super) struct Decoder {
    events: sse::Decoder,
    identity: Option<(ResponseId, ModelId)>,
    calls: BTreeMap<u32, OpenCall>,
    /// Whether the answer announced any tool call at all.
    ///
    /// The map above is emptied as each call closes, so it cannot answer "did this answer call a
    /// tool?" once the last call has ended — which is exactly when the finish reason is decided.
    called: bool,
    finished: bool,
    failed: bool,
}

impl Decoder {
    /// Creates a decoder that has not seen the response's first event yet.
    pub(super) fn new() -> Self {
        Self {
            events: sse::Decoder::new(),
            identity: None,
            calls: BTreeMap::new(),
            called: false,
            finished: false,
            failed: false,
        }
    }

    /// Turns the events one chunk of bytes completed into canonical events.
    fn absorb(&mut self, events: Vec<sse::Event>) -> Result<Vec<StreamEvent>, ProviderError> {
        let mut out = Vec::new();
        for event in events {
            if self.finished || self.failed {
                break;
            }
            // A heartbeat says nothing, and so does this dialect's absent `[DONE]` sentinel on a
            // compatible endpoint: the terminator here is `response.completed`.
            if event.is_done() || event.data.trim().is_empty() {
                continue;
            }
            let wire: EventWire = parse(event.data.as_bytes(), "a stream event")?;
            self.event(wire, &mut out)?;
        }
        Ok(out)
    }

    /// Dispatches one event by its `type` member.
    fn event(
        &mut self,
        mut wire: EventWire,
        out: &mut Vec<StreamEvent>,
    ) -> Result<(), ProviderError> {
        let kind = std::mem::take(&mut wire.kind);
        match kind.as_str() {
            "response.created" => self.created(wire, out),
            "response.output_item.added" => self.item_added(wire),
            "response.output_text.delta" => self.text_delta(wire, out),
            "response.reasoning_summary_text.delta" => self.reasoning_delta(wire, out),
            "response.function_call_arguments.delta" => self.arguments_delta(wire, out),
            "response.function_call_arguments.done" => self.arguments_done(wire, out),
            "response.completed" => self.completed(wire, out),
            "response.failed" => self.failed(wire, out),
            "error" => {
                out.push(StreamEvent::Error(
                    crate::provider::openai::error::stream_failure(wire.code.as_ref()),
                ));
                self.failed = true;
                Ok(())
            }
            // `response.in_progress`, the content-part and item wrappers, and every other event are
            // framing rather than content.
            _ => Ok(()),
        }
    }

    /// Emits the start event, and refuses a stream that announces its response twice.
    fn created(
        &mut self,
        wire: EventWire,
        out: &mut Vec<StreamEvent>,
    ) -> Result<(), ProviderError> {
        if self.identity.is_some() {
            return Err(malformed(
                "the stream announced its response more than once",
            ));
        }
        let response = wire
            .response
            .ok_or_else(|| malformed("the stream's first event carries no response"))?;
        let (id, model) = response.identity()?;
        self.identity = Some((id.clone(), model.clone()));
        out.push(StreamEvent::Start { id, model });
        Ok(())
    }

    /// Records the call an announced output item names.
    ///
    /// The item event is where a call's name first appears; its arguments follow as separate events
    /// addressed by `output_index`, so the entry is keyed by that index and nothing is emitted until
    /// the arguments start.
    fn item_added(&mut self, wire: EventWire) -> Result<(), ProviderError> {
        self.ready()?;
        let index = self.index(wire.output_index)?;
        let item = wire
            .item
            .ok_or_else(|| malformed("an output item event carries no item"))?;
        match item.kind() {
            "function_call" => {
                let (call_id, name) = item.function_call().ok_or_else(|| {
                    malformed("a streamed tool call announces no call id and no name")
                })?;
                if self.calls.contains_key(&index) {
                    return Err(malformed("the stream reused an output index"));
                }
                // The peer picks every index, so the number of open calls is the peer's choice.
                if self.calls.len() as u32 >= MAX_OUTPUT_ITEMS {
                    return Err(malformed(
                        "the stream opened more output items than this build serves",
                    ));
                }
                let id = ToolCallId::try_new(call_id).map_err(super::schema_failure)?;
                let name = ToolName::try_new(name).map_err(super::schema_failure)?;
                self.calls.insert(
                    index,
                    OpenCall {
                        id,
                        name,
                        started: false,
                    },
                );
                self.called = true;
                Ok(())
            }
            // A message's and a reasoning item's content arrive as their own delta events.
            "message" | "reasoning" => Ok(()),
            _ => Err(malformed(
                "the stream announced an output item this adapter cannot express",
            )),
        }
    }

    /// Emits one visible text fragment.
    fn text_delta(
        &mut self,
        wire: EventWire,
        out: &mut Vec<StreamEvent>,
    ) -> Result<(), ProviderError> {
        self.ready()?;
        if self.finished {
            return Err(malformed("the stream carried content after it completed"));
        }
        if let Some(text) = wire.delta.filter(|text| !text.is_empty()) {
            out.push(StreamEvent::TextDelta(text));
        }
        Ok(())
    }

    /// Emits one reasoning-summary fragment.
    fn reasoning_delta(
        &mut self,
        wire: EventWire,
        out: &mut Vec<StreamEvent>,
    ) -> Result<(), ProviderError> {
        self.ready()?;
        if self.finished {
            return Err(malformed("the stream carried content after it completed"));
        }
        if let Some(text) = wire.delta.filter(|text| !text.is_empty()) {
            out.push(StreamEvent::ReasoningDelta(text));
        }
        Ok(())
    }

    /// Emits one argument fragment, starting the call when this is its first fragment.
    fn arguments_delta(
        &mut self,
        wire: EventWire,
        out: &mut Vec<StreamEvent>,
    ) -> Result<(), ProviderError> {
        self.ready()?;
        if self.finished {
            return Err(malformed("the stream carried content after it completed"));
        }
        let index = self.index(wire.output_index)?;
        let delta = wire.delta.filter(|text| !text.is_empty());
        let Some(call) = self.calls.get_mut(&index) else {
            return Err(malformed("a tool call fragment names no announced call"));
        };
        if !call.started {
            call.started = true;
            out.push(StreamEvent::ToolCallStart {
                id: call.id.clone(),
                name: call.name.clone(),
                index,
            });
        }
        if let Some(arguments) = delta {
            out.push(StreamEvent::ToolCallDelta {
                id: call.id.clone(),
                arguments,
                index,
            });
        }
        Ok(())
    }

    /// Closes one call.
    ///
    /// A `done` event repeats the whole argument string the deltas already carried, so the value is
    /// written only when no fragment arrived: writing both would hand the client the same JSON twice,
    /// and a call that never streamed a fragment (an empty argument object) would otherwise produce a
    /// start and an end with nothing between them.
    fn arguments_done(
        &mut self,
        wire: EventWire,
        out: &mut Vec<StreamEvent>,
    ) -> Result<(), ProviderError> {
        self.ready()?;
        let index = self.index(wire.output_index)?;
        let Some(call) = self.calls.remove(&index) else {
            return Err(malformed("a tool call ended without having been announced"));
        };
        if !call.started {
            out.push(StreamEvent::ToolCallStart {
                id: call.id.clone(),
                name: call.name.clone(),
                index,
            });
            if let Some(arguments) = wire.arguments.filter(|text| !text.is_empty()) {
                out.push(StreamEvent::ToolCallDelta {
                    id: call.id.clone(),
                    arguments,
                    index,
                });
            }
        }
        out.push(StreamEvent::ToolCallEnd { id: call.id, index });
        Ok(())
    }

    /// Reports usage and why generation stopped, after closing every open call.
    fn completed(
        &mut self,
        wire: EventWire,
        out: &mut Vec<StreamEvent>,
    ) -> Result<(), ProviderError> {
        self.ready()?;
        let response = wire
            .response
            .ok_or_else(|| malformed("the completed event carries no response"))?;
        let has_calls = self.called;
        // A stop ends every call that is still being assembled; a call without its end event would
        // leave a client waiting.
        for (index, call) in std::mem::take(&mut self.calls) {
            out.push(StreamEvent::ToolCallEnd { id: call.id, index });
        }
        let finish = response.finish(has_calls)?;
        if let Some(usage) = response.stream_usage() {
            out.push(StreamEvent::Usage(usage));
        }
        out.push(StreamEvent::Finish(finish));
        self.finished = true;
        Ok(())
    }

    /// Reports a failed answer.
    fn failed(&mut self, wire: EventWire, out: &mut Vec<StreamEvent>) -> Result<(), ProviderError> {
        let code = wire
            .response
            .as_ref()
            .and_then(|response| response.error_code());
        out.push(StreamEvent::Error(
            crate::provider::openai::error::stream_failure(code),
        ));
        self.failed = true;
        Ok(())
    }

    /// Returns the peer-chosen index of an item event.
    fn index(&self, index: Option<u32>) -> Result<u32, ProviderError> {
        index.ok_or_else(|| malformed("an output item event names no output index"))
    }

    /// Refuses content that precedes the response it belongs to.
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
        if self.finished || self.failed {
            return Ok(Vec::new());
        }
        let events = self.events.push(chunk).map_err(sse_failure)?;
        self.absorb(events)
    }

    fn finish(&mut self) -> Result<Vec<StreamEvent>, ProviderError> {
        if self.finished || self.failed {
            return Ok(Vec::new());
        }
        let events = self.events.finish().map_err(sse_failure)?;
        let out = self.absorb(events)?;
        if self.finished || self.failed {
            return Ok(out);
        }
        // A body that ended before the completed event cannot be reported as a finished answer,
        // whether or not a compatible endpoint sent a `[DONE]` sentinel.
        Err(malformed("the stream ended without a completed response"))
    }
}

/// One tool call still being assembled.
struct OpenCall {
    id: ToolCallId,
    name: ToolName,
    /// Whether `ToolCallStart` has been emitted for this call.
    started: bool,
}

/// One event of the stream.
///
/// The event kinds share one struct because the dialect tags them with a string member rather than by
/// shape. The members a kind does not use are absent, and the handler for that kind is what requires
/// the ones it needs. `sequence_number`, `item_id` and the content indexes are this protocol's
/// framing: they are read into `_extra` and play no part in a canonical event.
#[derive(Deserialize)]
struct EventWire {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    response: Option<ResponseWire>,
    #[serde(default)]
    item: Option<OutputItemWire>,
    #[serde(default)]
    output_index: Option<u32>,
    #[serde(default)]
    delta: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
    #[serde(default)]
    code: Option<RawJson>,
    #[serde(flatten)]
    _extra: BTreeMap<String, RawJson>,
}

/// Wraps a framing failure as the adapter's malformed-document error.
fn sse_failure(error: sse::SseError) -> ProviderError {
    malformed(format!(
        "the response stream is not a valid event stream: {error}"
    ))
}
