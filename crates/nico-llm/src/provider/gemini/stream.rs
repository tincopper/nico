//! Decoding the `streamGenerateContent` event stream into canonical events.
//!
//! One decoder serves one response: it holds the event framing's partial frame, the identity the first
//! chunk announced, the position of the next tool call, and whether the answer has finished. A shared
//! decoder would let one response's partial frame be completed by another one's bytes, which is why
//! [`Provider::stream_decoder`](crate::Provider::stream_decoder) creates a fresh value per response.
//!
//! # One chunk equals one answer fragment
//!
//! Each event's payload is a whole `GenerateContentResponse`, so a chunk carries a list of parts rather
//! than a delta field: text and thought parts become [`StreamEvent::TextDelta`] and
//! [`StreamEvent::ReasoningDelta`], and a `functionCall` part — which this dialect sends complete —
//! becomes a start, one delta carrying the whole arguments document, and an end. The start/delta/end
//! triple exists because the canonical stream carries a call's arguments in a delta: the client codec
//! accumulates them and writes the object once the call ends, and the dialect having no fragments does
//! not change where the bytes must sit.
//!
//! A part with no call id gets one this adapter assigns, `call_` plus the call's position among the
//! calls of the same answer, so a client can answer a call whose id the provider never sent.
//!
//! # The finish reason is the completeness mark
//!
//! `finishReason` says why generation stopped, and it is what makes a stream complete: a body that
//! ends before one is refused, because the caller must not be told the model finished when nobody said
//! it did. Once the reason arrives the answer is over and every later chunk is ignored — a second
//! answer is not part of this one. `usageMetadata` is written before the finish event of the chunk that
//! carries it, so the caller reads the answer's accounting before its end.
//!
//! A prompt the provider blocked before generating is reported as its own finish event
//! (`ContentFilter`): the chunk carries no candidate at all, and turning that into a missing finish
//! reason would report an unreadable answer instead of a filtered one.
//!
//! # Candidates and envelopes
//!
//! The canonical stream addresses one answer, so a chunk with more than one candidate, or a candidate
//! whose `index` is not zero, is refused rather than merged: the events carry no choice index, and
//! merging would attribute one candidate's text to another. The envelope's other members are consumed
//! (see [`super::response`]); an unknown member *inside a part* is refused, because a part is content.
//!
//! A chunk that repeats the response id or model version is accepted when it repeats the same value and
//! refused when it names a different one: an answer that changes identity halfway is two answers.

use super::{
    CALL_ID_PREFIX, DEFAULT_RESPONSE_ID, malformed, parse,
    parts::{DecodedPart, PartIn},
    response::{self, ResponseWire},
    schema_failure, usage,
};
use crate::{
    provider::{ProviderError, ProviderStream},
    schema::{FinishReason, ModelId, ResponseId, StreamEvent, ToolCallId, ToolName},
    stream::sse,
};

/// The decoder for one streamed response.
pub(super) struct Decoder {
    events: sse::Decoder,
    identity: Option<(ResponseId, ModelId)>,
    /// The position the next tool call of this answer gets.
    calls: u32,
    finished: bool,
    done: bool,
}

impl Decoder {
    /// Creates a decoder that has not seen the response's first chunk yet.
    pub(super) fn new() -> Self {
        Self {
            events: sse::Decoder::new(),
            identity: None,
            calls: 0,
            finished: false,
            done: false,
        }
    }

    /// Turns the events one chunk of bytes completed into canonical events.
    fn absorb(&mut self, events: Vec<sse::Event>) -> Result<Vec<StreamEvent>, ProviderError> {
        let mut out = Vec::new();
        for event in events {
            if self.done {
                break;
            }
            // A heartbeat carries no payload, and `[DONE]` is not part of this dialect: the finish
            // reason ends the answer, and a compatible endpoint's sentinel adds no fact.
            if event.is_done() || event.data.trim().is_empty() {
                continue;
            }
            let wire: ResponseWire = parse(event.data.as_bytes(), "a stream event")?;
            self.chunk(wire, &mut out)?;
        }
        Ok(out)
    }

    /// Turns one chunk into canonical events.
    fn chunk(
        &mut self,
        wire: ResponseWire,
        out: &mut Vec<StreamEvent>,
    ) -> Result<(), ProviderError> {
        self.identify(&wire, out)?;
        // The chunk's counts describe the answer this chunk completes, so they are written after its
        // content and before its end: a client reads the accounting before the end of the answer.
        let counts = wire.usage.as_ref().map(usage::decode);
        if wire.prompt_feedback.as_ref().is_some_and(|it| it.blocked()) {
            out.extend(counts.map(StreamEvent::Usage));
            return self.finish(FinishReason::ContentFilter, out);
        }
        if wire.candidates.len() > 1 {
            return Err(malformed(
                "the answer carries more than one candidate, which the canonical stream cannot address",
            ));
        }
        let mut reason = None;
        for candidate in wire.candidates {
            if candidate.index.is_some_and(|index| index != 0) {
                return Err(malformed(
                    "the answer carries a candidate the canonical stream cannot address",
                ));
            }
            if let Some(content) = candidate.content {
                if let Some(role) = &content.role
                    && role != "model"
                {
                    return Err(malformed("the answer's content is not a model turn"));
                }
                for part in content.into_parts()? {
                    self.part(part, out)?;
                }
            }
            if let Some(wire) = candidate.finish_reason {
                reason = Some(response::finish_reason(&wire)?);
            }
        }
        out.extend(counts.map(StreamEvent::Usage));
        match reason {
            Some(reason) => self.finish(reason, out),
            None => Ok(()),
        }
    }

    /// Emits the start event, or checks the identity a later chunk repeats.
    fn identify(
        &mut self,
        wire: &ResponseWire,
        out: &mut Vec<StreamEvent>,
    ) -> Result<(), ProviderError> {
        let Some((id, model)) = &self.identity else {
            // The canonical answer names the model that produced it; this dialect reports it per
            // chunk, and a made-up name would be a lie in the answer and in the `llm.model` field.
            let version = wire
                .model_version
                .as_deref()
                .ok_or_else(|| malformed("the answer names no model version"))?;
            let model = ModelId::try_new(version).map_err(schema_failure)?;
            let id =
                ResponseId::try_new(wire.response_id.as_deref().unwrap_or(DEFAULT_RESPONSE_ID))
                    .map_err(schema_failure)?;
            self.identity = Some((id.clone(), model.clone()));
            out.push(StreamEvent::Start { id, model });
            return Ok(());
        };
        if let Some(version) = &wire.model_version
            && version != model.as_str()
        {
            return Err(malformed("the stream changed the model it names"));
        }
        if let Some(response) = &wire.response_id
            && response != id.as_str()
        {
            return Err(malformed("the stream changed the response it names"));
        }
        Ok(())
    }

    /// Turns one part into the canonical increment it carries.
    fn part(&mut self, part: PartIn, out: &mut Vec<StreamEvent>) -> Result<(), ProviderError> {
        match part.into_part()? {
            DecodedPart::Text(text) => {
                // A zero-length increment adds nothing to the answer; it is not content being
                // dropped, which is why it is skipped instead of refused.
                if !text.is_empty() {
                    out.push(StreamEvent::TextDelta(text));
                }
            }
            DecodedPart::Thought(text) => {
                if !text.is_empty() {
                    out.push(StreamEvent::ReasoningDelta(text));
                }
            }
            DecodedPart::Call {
                id,
                name,
                arguments,
            } => {
                let index = self.calls;
                self.calls = self.calls.checked_add(1).ok_or_else(|| {
                    malformed("the answer carries more tool calls than this adapter can address")
                })?;
                let id = id.unwrap_or_else(|| format!("{CALL_ID_PREFIX}{index}"));
                let id = ToolCallId::try_new(id).map_err(schema_failure)?;
                let name = ToolName::try_new(name).map_err(schema_failure)?;
                out.push(StreamEvent::ToolCallStart {
                    id: id.clone(),
                    name,
                    index,
                });
                out.push(StreamEvent::ToolCallDelta {
                    id: id.clone(),
                    arguments: arguments.to_text(),
                    index,
                });
                out.push(StreamEvent::ToolCallEnd { id, index });
            }
        }
        Ok(())
    }

    /// Ends the answer with one finish reason.
    fn finish(
        &mut self,
        reason: FinishReason,
        out: &mut Vec<StreamEvent>,
    ) -> Result<(), ProviderError> {
        if self.finished {
            return Err(malformed(
                "the stream reported a finish reason more than once",
            ));
        }
        out.push(StreamEvent::Finish(reason));
        self.finished = true;
        self.done = true;
        Ok(())
    }
}

impl ProviderStream for Decoder {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<StreamEvent>, ProviderError> {
        if self.done {
            return Ok(Vec::new());
        }
        let events = self.events.push(chunk).map_err(sse_failure)?;
        self.absorb(events)
    }

    fn finish(&mut self) -> Result<Vec<StreamEvent>, ProviderError> {
        if self.done && self.finished {
            return Ok(Vec::new());
        }
        let events = self.events.finish().map_err(sse_failure)?;
        let out = self.absorb(events)?;
        // A body that ended before a finish reason cannot be reported as a finished answer.
        if !self.finished {
            return Err(malformed("the stream ended without a finish reason"));
        }
        Ok(out)
    }
}

/// Wraps a framing failure as the adapter's malformed-document error.
fn sse_failure(error: sse::SseError) -> ProviderError {
    malformed(format!(
        "the response stream is not a valid event stream: {error}"
    ))
}
