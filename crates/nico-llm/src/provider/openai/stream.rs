//! Decoding a Chat Completions event stream into canonical events.
//!
//! One decoder serves one response: it holds the event framing's partial frame, the response identity
//! the first chunk announced, the tool calls still being assembled, and whether the stream has
//! finished. A shared decoder would let one response's partial frame be completed by another one's
//! bytes, which is why [`Provider::stream_decoder`](crate::Provider::stream_decoder) creates a fresh
//! value per response.
//!
//! # Event order
//!
//! `Start` is emitted once, on the first chunk that names the response. Text, reasoning and tool-call
//! fragments follow in the order the provider sent them; a fragment that is only a role marker or an
//! empty string produces no canonical event, because it adds no bytes to the answer. A finish reason
//! closes every open tool call first ([`StreamEvent::ToolCallEnd`]) in index order, then emits
//! [`StreamEvent::Finish`]. Usage is emitted from whichever chunk carries it, before or after the
//! finish event; a provider may report it in a final chunk whose `choices` are empty.
//!
//! # Truncation is never completeness
//!
//! A finish reason is what makes a stream complete: it is the one fact every wire form of this
//! protocol carries. The `[DONE]` sentinel is the protocol's redundant terminator, and a body may end
//! without it — compatible endpoints do — so its absence is not treated as a truncation. A body that
//! ends before a finish reason is refused, because the decoder cannot tell the caller why generation
//! stopped: the client must not be told the model finished when nobody said it did.
//!
//! A failed frame (`{"error": …}`) becomes [`StreamEvent::Error`] and ends the decoder's output. The
//! provider's message and type are not read; only a bounded printable `code` is kept for the log.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::{
    MAX_TOOL_CALLS, error, finish_reason, malformed, parse, reject_extra,
    usage::{self, UsageWire},
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
    finished: bool,
    done: bool,
    failed: bool,
}

impl Decoder {
    /// Creates a decoder that has not seen the response's first chunk yet.
    pub(super) fn new() -> Self {
        Self {
            events: sse::Decoder::new(),
            identity: None,
            calls: BTreeMap::new(),
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
            if event.is_done() {
                self.done = true;
                continue;
            }
            // A heartbeat carries no payload; an event with nothing but whitespace says nothing.
            if event.data.trim().is_empty() {
                continue;
            }
            let chunk: ChunkWire = parse(event.data.as_bytes(), "a stream chunk")?;
            if let Some(error) = chunk.error.as_ref() {
                out.push(StreamEvent::Error(error::stream_failure(
                    error.code.as_ref(),
                )));
                self.failed = true;
                break;
            }
            if let Some(start) = self.start(&chunk)? {
                out.push(start);
            }
            for (position, choice) in chunk.choices.into_iter().enumerate() {
                self.choice(position, choice, &mut out)?;
            }
            // Written after the choices so a chunk that carries a finish reason and usage in the same
            // document emits them in the order a client reads them.
            if let Some(usage) = &chunk.usage {
                out.push(StreamEvent::Usage(usage::decode(usage)));
            }
        }
        Ok(out)
    }

    /// Emits the start event once, and refuses a chunk that contradicts the identity it announced.
    ///
    /// A chunk may repeat the identity, omit it, or repeat only one part of it: a missing field is
    /// not a contradiction. What is refused is a present field that disagrees with what the stream
    /// already announced, because two answers under one stream would leave the client's chunks
    /// describing a response that is not the one it is reading.
    fn start(&mut self, chunk: &ChunkWire) -> Result<Option<StreamEvent>, ProviderError> {
        let id = chunk
            .id
            .as_deref()
            .map(|id| ResponseId::try_new(id).map_err(super::schema_failure))
            .transpose()?;
        let model = chunk
            .model
            .as_deref()
            .map(|model| ModelId::try_new(model).map_err(super::schema_failure))
            .transpose()?;
        match &self.identity {
            Some((known_id, known_model)) => {
                let contradicts = id.as_ref().is_some_and(|id| id != known_id)
                    || model.as_ref().is_some_and(|model| model != known_model);
                if contradicts {
                    return Err(malformed(
                        "the stream changed the response identity it announced",
                    ));
                }
                Ok(None)
            }
            None => {
                let (Some(id), Some(model)) = (id, model) else {
                    return Err(malformed(
                        "the first stream chunk names no response and its model",
                    ));
                };
                self.identity = Some((id.clone(), model.clone()));
                Ok(Some(StreamEvent::Start { id, model }))
            }
        }
    }

    /// Decodes one candidate answer of a chunk.
    ///
    /// The canonical stream has one answer, because a request that would produce several is refused by
    /// the client codec. A provider that streams a second choice is reporting something this model
    /// cannot address, so it is refused rather than silently folded into the first choice.
    fn choice(
        &mut self,
        position: usize,
        wire: StreamChoiceWire,
        out: &mut Vec<StreamEvent>,
    ) -> Result<(), ProviderError> {
        reject_extra(&wire.extra, "choices[]")?;
        if position > 0 || wire.index != 0 {
            return Err(malformed("the stream carries more than one choice"));
        }
        self.delta(wire.delta, out)?;
        if let Some(reason) = wire.finish_reason {
            self.finish_event(&reason, out)?;
        }
        Ok(())
    }

    /// Decodes one incremental payload.
    fn delta(&mut self, wire: DeltaWire, out: &mut Vec<StreamEvent>) -> Result<(), ProviderError> {
        reject_extra(&wire.extra, "choices[].delta")?;
        if let Some(role) = &wire.role {
            if role != "assistant" {
                return Err(malformed("the stream produced a non-assistant message"));
            }
        }
        let text = wire.content.filter(|text| !text.is_empty());
        let reasoning = wire.reasoning_content.filter(|text| !text.is_empty());
        let calls = wire.tool_calls.unwrap_or_default();
        if self.finished {
            if text.is_some() || reasoning.is_some() || !calls.is_empty() {
                return Err(malformed(
                    "the stream carried content after its finish reason",
                ));
            }
            return Ok(());
        }
        if let Some(text) = text {
            out.push(StreamEvent::TextDelta(text));
        }
        if let Some(text) = reasoning {
            out.push(StreamEvent::ReasoningDelta(text));
        }
        for call in calls {
            self.tool_call(call, out)?;
        }
        Ok(())
    }

    /// Decodes one tool-call fragment, assigning a canonical event per stage of the call.
    fn tool_call(
        &mut self,
        wire: ToolCallDeltaWire,
        out: &mut Vec<StreamEvent>,
    ) -> Result<(), ProviderError> {
        reject_extra(&wire.extra, "choices[].delta.tool_calls[]")?;
        if let Some(kind) = &wire.kind {
            if kind != "function" {
                return Err(malformed(
                    "the stream carries a tool call this adapter cannot express",
                ));
            }
        }
        let index = wire.index;
        let known = self.calls.get(&index).cloned();
        let mut function = wire.function;
        if let Some(function) = function.as_mut() {
            reject_extra(&function.extra, "choices[].delta.tool_calls[].function")?;
        }
        if let Some(call) = known {
            if let Some(id) = &wire.id {
                if id != call.id.as_str() {
                    return Err(malformed("a tool call fragment changed its id"));
                }
            }
            if let Some(function) = function {
                if let Some(name) = &function.name {
                    if name != call.name.as_str() {
                        return Err(malformed("a tool call fragment changed its name"));
                    }
                }
                if let Some(arguments) = function.arguments.filter(|text| !text.is_empty()) {
                    out.push(StreamEvent::ToolCallDelta {
                        id: call.id,
                        arguments,
                        index,
                    });
                }
            }
            return Ok(());
        }
        let id = wire
            .id
            .ok_or_else(|| malformed("a tool call fragment starts without an id"))?;
        let function =
            function.ok_or_else(|| malformed("a tool call fragment starts without a function"))?;
        let name = function
            .name
            .ok_or_else(|| malformed("a tool call fragment starts without a name"))?;
        // A fragment may name any `index`, so the number of open calls is the peer's choice.
        if self.calls.len() >= MAX_TOOL_CALLS {
            return Err(malformed(
                "the stream opened more tool calls than this build serves",
            ));
        }
        let id = ToolCallId::try_new(id).map_err(super::schema_failure)?;
        let name = ToolName::try_new(name).map_err(super::schema_failure)?;
        self.calls.insert(
            index,
            OpenCall {
                id: id.clone(),
                name: name.clone(),
            },
        );
        out.push(StreamEvent::ToolCallStart {
            id: id.clone(),
            name,
            index,
        });
        if let Some(arguments) = function.arguments.filter(|text| !text.is_empty()) {
            out.push(StreamEvent::ToolCallDelta {
                id,
                arguments,
                index,
            });
        }
        Ok(())
    }

    /// Closes the open tool calls and reports why generation stopped.
    fn finish_event(
        &mut self,
        reason: &str,
        out: &mut Vec<StreamEvent>,
    ) -> Result<(), ProviderError> {
        if self.finished {
            return Err(malformed(
                "the stream reported a finish reason more than once",
            ));
        }
        let reason = finish_reason(reason)?;
        // A stop ends every call that was still being assembled; the protocol marks the end by the
        // next call or by the finish, and a call without its end event would leave a client waiting.
        for (index, call) in std::mem::take(&mut self.calls) {
            out.push(StreamEvent::ToolCallEnd { id: call.id, index });
        }
        out.push(StreamEvent::Finish(reason));
        self.finished = true;
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
        // or not the provider sent its redundant `[DONE]` sentinel.
        if !self.finished {
            return Err(malformed("the stream ended without a finish reason"));
        }
        Ok(out)
    }
}

/// One tool call still being assembled.
#[derive(Clone)]
struct OpenCall {
    id: ToolCallId,
    name: ToolName,
}

/// A stream chunk.
///
/// `object` and `created` describe the provider's framing and are read without inspection: the
/// adapter does not use them, so a value it never reads must not be able to fail the document.
#[derive(Deserialize)]
struct ChunkWire {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default, rename = "object")]
    _object: Option<RawJson>,
    #[serde(default, rename = "created")]
    _created: Option<RawJson>,
    #[serde(default)]
    choices: Vec<StreamChoiceWire>,
    #[serde(default)]
    usage: Option<UsageWire>,
    #[serde(default)]
    error: Option<ErrorBodyWire>,
    // A chunk's unknown top-level field has no canonical event to ride on. The complete answer's
    // extensions are decoded from the non-streamed document; a stream chunk is a fragment, and
    // inventing an extension event would be a shape no client protocol asked for.
    #[serde(flatten)]
    _extra: BTreeMap<String, RawJson>,
}

/// One candidate answer of a chunk.
#[derive(Deserialize)]
struct StreamChoiceWire {
    index: u32,
    #[serde(default)]
    delta: DeltaWire,
    #[serde(default)]
    finish_reason: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

/// The increment a chunk carries.
#[derive(Default, Deserialize)]
struct DeltaWire {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallDeltaWire>>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

/// One tool call's fragment.
#[derive(Deserialize)]
struct ToolCallDeltaWire {
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    function: Option<FunctionDeltaWire>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

/// A tool call's name (first fragment) and arguments (later fragments).
#[derive(Default, Deserialize)]
struct FunctionDeltaWire {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}

/// The provider's failure object inside a stream frame.
///
/// Only `code` is read. `message`, `type` and `param` are the provider's diagnostic and are never
/// forwarded; serde ignores them because this type does not declare them.
#[derive(Deserialize)]
struct ErrorBodyWire {
    #[serde(default)]
    code: Option<RawJson>,
}

/// Wraps a framing failure as the adapter's malformed-document error.
fn sse_failure(error: sse::SseError) -> ProviderError {
    malformed(format!(
        "the response stream is not a valid event stream: {error}"
    ))
}
