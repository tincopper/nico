//! Encoding canonical stream events as Chat Completions chunks.
//!
//! A chunk repeats the response id, model and creation time the start event announced, so the encoder
//! keeps them for the life of one response: that is what makes [`super::OpenAiChatCodec`] stateless
//! while a stream still carries the facts a client reads from every chunk.
//!
//! # Shapes that have no chunk of their own
//!
//! - [`StreamEvent::ToolCallEnd`] writes nothing: the protocol marks a call's arguments complete by
//!   starting the next one or by the finish chunk, and inventing an empty fragment would be a change
//!   to the arguments document.
//! - [`StreamEvent::Usage`] is written after the finish chunk, because a client reads usage from the
//!   chunk that follows the finish reason. A provider may report it before or after its finish
//!   event, so the encoder holds it in the first case and writes it immediately in the second. It is
//!   only written when the provider reported counts; a gateway estimate is not a bill.
//! - The terminator is written when the response body ends, not when the finish event arrives, so a
//!   usage chunk that follows the finish reason still lands before it.
//! - A stream that fails ([`StreamEvent::Error`]) ends with the error frame and no terminator: the
//!   body simply stops, which is how a client learns the answer was cut short.

use serde::Serialize;

use super::{CHUNK_OBJECT, CREATED, PROTOCOL, error, finish_reason, usage};
use crate::{
    protocol::{ClientStreamEncoder, CodecError},
    schema::{FinishReason, StreamEvent},
    stream::sse,
};

/// The encoder for one response stream.
#[derive(Debug, Default)]
pub(super) struct Encoder {
    id: Option<String>,
    model: Option<String>,
    usage: Option<usage::UsageWire>,
    finished: bool,
    terminated: bool,
}

impl Encoder {
    /// Creates an encoder that has not seen the stream's start event yet.
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Writes one chunk that carries a delta.
    fn delta(&self, delta: Delta<'_>) -> Result<Vec<u8>, CodecError> {
        self.chunk(
            vec![ChunkChoice {
                index: 0,
                delta,
                finish_reason: None,
            }],
            None,
        )
    }

    /// Writes one chunk, refusing an event that arrived before the start event.
    fn chunk(
        &self,
        choices: Vec<ChunkChoice<'_>>,
        usage: Option<usage::UsageWire>,
    ) -> Result<Vec<u8>, CodecError> {
        let (Some(id), Some(model)) = (self.id.as_deref(), self.model.as_deref()) else {
            return Err(CodecError::Encode {
                protocol: PROTOCOL,
                reason: "a stream event arrived before the start event".to_owned(),
            });
        };
        let wire = ChunkWire {
            id,
            object: CHUNK_OBJECT,
            created: CREATED,
            model,
            choices,
            usage,
        };
        let body = serde_json::to_string(&wire).map_err(|error| CodecError::Encode {
            protocol: PROTOCOL,
            reason: error.to_string(),
        })?;
        Ok(sse::encode_event(&body))
    }

    /// Writes the finish chunk and any usage that arrived before it.
    fn finish_stream(&mut self, reason: FinishReason) -> Result<Vec<u8>, CodecError> {
        let mut bytes = self.chunk(
            vec![ChunkChoice {
                index: 0,
                delta: Delta::default(),
                finish_reason: Some(finish_reason(reason)),
            }],
            None,
        )?;
        if let Some(usage) = self.usage.take() {
            bytes.extend_from_slice(&self.chunk(Vec::new(), Some(usage))?);
        }
        self.finished = true;
        Ok(bytes)
    }
}

impl ClientStreamEncoder for Encoder {
    fn push(&mut self, event: &StreamEvent) -> Result<Vec<u8>, CodecError> {
        // Once the stream has ended nothing more is written; after the finish chunk only a usage
        // report is still expected, because a provider may send it after its finish event.
        if self.terminated || (self.finished && !matches!(event, StreamEvent::Usage(_))) {
            return Ok(Vec::new());
        }
        match event {
            StreamEvent::Start { id, model } => {
                self.id = Some(id.as_str().to_owned());
                self.model = Some(model.as_str().to_owned());
                self.delta(Delta {
                    role: Some("assistant"),
                    content: Some(""),
                    ..Delta::default()
                })
            }
            StreamEvent::TextDelta(text) => self.delta(Delta::text(text)),
            StreamEvent::ReasoningDelta(text) => self.delta(Delta::reasoning(text)),
            StreamEvent::ToolCallStart { id, name, index } => {
                self.delta(Delta::tool_call(ToolCallDelta {
                    index: *index,
                    id: Some(id.as_str()),
                    kind: Some("function"),
                    function: Some(FunctionDelta {
                        name: Some(name.as_str()),
                        arguments: Some(""),
                    }),
                }))
            }
            StreamEvent::ToolCallDelta {
                arguments, index, ..
            } => self.delta(Delta::tool_call(ToolCallDelta {
                index: *index,
                id: None,
                kind: None,
                function: Some(FunctionDelta {
                    name: None,
                    arguments: Some(arguments),
                }),
            })),
            StreamEvent::ToolCallEnd { .. } => Ok(Vec::new()),
            StreamEvent::Usage(usage) => {
                let encoded = usage::encode(usage);
                if self.finished {
                    return match encoded {
                        Some(usage) => self.chunk(Vec::new(), Some(usage)),
                        None => Ok(Vec::new()),
                    };
                }
                self.usage = encoded;
                Ok(Vec::new())
            }
            StreamEvent::Finish(reason) => self.finish_stream(*reason),
            StreamEvent::Error(error) => {
                self.terminated = true;
                error::frame(error)
            }
        }
    }

    fn finish(&mut self) -> Result<Vec<u8>, CodecError> {
        if self.terminated {
            return Ok(Vec::new());
        }
        let mut bytes = Vec::new();
        if let Some(usage) = self.usage.take() {
            // A usage event without a start event cannot be framed with an id; the terminator is
            // still written, so the client is never left with an unterminated stream.
            if self.id.is_some() {
                bytes.extend_from_slice(&self.chunk(Vec::new(), Some(usage))?);
            }
        }
        bytes.extend_from_slice(&sse::encode_done());
        self.terminated = true;
        Ok(bytes)
    }
}

/// One streamed chunk.
#[derive(Serialize)]
struct ChunkWire<'a> {
    id: &'a str,
    object: &'static str,
    created: u64,
    model: &'a str,
    choices: Vec<ChunkChoice<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<usage::UsageWire>,
}

/// One candidate of a chunk; a usage-only chunk has none.
#[derive(Serialize)]
struct ChunkChoice<'a> {
    index: u32,
    delta: Delta<'a>,
    finish_reason: Option<&'static str>,
}

/// The increment a chunk carries.
#[derive(Serialize, Default)]
struct Delta<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCallDelta<'a>>>,
}

impl<'a> Delta<'a> {
    /// A visible-text delta.
    fn text(text: &'a str) -> Self {
        Self {
            content: Some(text),
            ..Self::default()
        }
    }

    /// A hidden-reasoning delta.
    fn reasoning(text: &'a str) -> Self {
        Self {
            reasoning_content: Some(text),
            ..Self::default()
        }
    }

    /// A delta carrying one tool-call fragment.
    fn tool_call(call: ToolCallDelta<'a>) -> Self {
        Self {
            tool_calls: Some(vec![call]),
            ..Self::default()
        }
    }
}

/// One tool call's fragment; `index` is the position the call occupies in the answer.
#[derive(Serialize)]
struct ToolCallDelta<'a> {
    index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<&'a str>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    kind: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    function: Option<FunctionDelta<'a>>,
}

/// A tool call's name (first fragment) and arguments (later fragments).
#[derive(Serialize)]
struct FunctionDelta<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    arguments: Option<&'a str>,
}
