//! Encoding canonical stream events as `GenerateContentResponse` chunks.
//!
//! One event, one chunk: this protocol has no delta members — a text increment is a whole `parts[]`
//! list on one chunk — so the encoder's only accumulated state is a tool call's arguments, which the
//! dialect cannot carry as fragments and must write as one object once the canonical stream closes
//! the call. The identity the first chunk carries (`responseId`, `modelVersion`) comes from the
//! canonical start event, and a chunk that carries no content is a legitimate document, which is
//! what makes usage writable on its own: a report that lands after the finish event is still
//! written before the stream ends, because this protocol has no terminator frame — the body simply
//! ends, and `finish()` writes nothing.
//!
//! # What is refused, and what is ignored
//!
//! A content event before the start event, a fragment for a call that has not started, and a call
//! whose arguments never closed are `Encode` failures: the canonical stream is well-formed by
//! contract, so reaching one of these is a gateway defect, not a client answer. Content after the
//! finish event is ignored rather than refused — the answer is over, and a second content event
//! cannot reopen it — while a usage event after the finish event is still written, because it
//! completes the accounting of the answer that just ended.

use serde::Serialize;

use super::{PROTOCOL, error, parts, response, usage};
use crate::{
    protocol::{ClientStreamEncoder, CodecError},
    schema::{RawJson, StreamEvent},
    stream::sse,
};

/// The encoder for one response stream.
#[derive(Debug, Default)]
pub(super) struct Encoder {
    started: bool,
    finished: bool,
    terminated: bool,
    calls: Vec<OpenCall>,
}

/// One call the stream is accumulating.
#[derive(Debug)]
struct OpenCall {
    /// The call's position in the canonical stream, which its fragments repeat.
    canonical: u32,
    /// The identifier the caller's next request must carry back.
    id: String,
    /// The tool being called.
    name: String,
    /// The argument fragments received so far.
    arguments: String,
}

impl Encoder {
    /// Creates an encoder that has not seen the stream's start event yet.
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Writes one chunk framing the whole document as a `data:` event.
    fn chunk<T: Serialize>(&self, body: &T) -> Result<Vec<u8>, CodecError> {
        let document = serde_json::to_string(body).map_err(|failure| CodecError::Encode {
            protocol: PROTOCOL,
            reason: failure.to_string(),
        })?;
        Ok(sse::encode_event(&document))
    }

    /// Writes one chunk that carries content parts.
    fn content_chunk(&self, wire_parts: Vec<parts::PartWire<'_>>) -> Result<Vec<u8>, CodecError> {
        self.chunk(&ChunkWire {
            candidates: Some(vec![CandidateWire {
                content: Some(parts::ContentOut {
                    role: parts::MODEL,
                    parts: wire_parts,
                }),
                finish_reason: None,
            }]),
            usage: None,
        })
    }

    /// Refuses a content event that arrived before the start event.
    fn require_started(&self) -> Result<(), CodecError> {
        if self.started {
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
        if self.terminated {
            return Ok(Vec::new());
        }
        match event {
            StreamEvent::Start { id, model } => {
                if self.started {
                    return Err(CodecError::Encode {
                        protocol: PROTOCOL,
                        reason: "the stream started twice".to_owned(),
                    });
                }
                self.started = true;
                self.chunk(&IdentityWire {
                    response_id: id.as_str(),
                    model_version: model.as_str(),
                })
            }
            StreamEvent::TextDelta(text) => {
                self.require_started()?;
                if self.finished || text.is_empty() {
                    return Ok(Vec::new());
                }
                self.content_chunk(vec![parts::PartOut::Text(text).into()])
            }
            StreamEvent::ReasoningDelta(text) => {
                self.require_started()?;
                if self.finished || text.is_empty() {
                    return Ok(Vec::new());
                }
                self.content_chunk(vec![
                    parts::PartOut::Thought {
                        text,
                        signature: None,
                    }
                    .into(),
                ])
            }
            StreamEvent::ToolCallStart { id, name, index } => {
                self.require_started()?;
                if self.finished {
                    return Ok(Vec::new());
                }
                if self.calls.iter().any(|call| call.canonical == *index) {
                    return Err(CodecError::Encode {
                        protocol: PROTOCOL,
                        reason: "a tool call started twice".to_owned(),
                    });
                }
                self.calls.push(OpenCall {
                    canonical: *index,
                    id: id.as_str().to_owned(),
                    name: name.as_str().to_owned(),
                    arguments: String::new(),
                });
                Ok(Vec::new())
            }
            StreamEvent::ToolCallDelta {
                arguments, index, ..
            } => {
                self.require_started()?;
                if self.finished {
                    return Ok(Vec::new());
                }
                let Some(call) = self.calls.iter_mut().find(|call| call.canonical == *index) else {
                    return Err(CodecError::Encode {
                        protocol: PROTOCOL,
                        reason: "a tool-call fragment arrived before its call started".to_owned(),
                    });
                };
                call.arguments.push_str(arguments);
                Ok(Vec::new())
            }
            StreamEvent::ToolCallEnd { index, .. } => {
                self.require_started()?;
                if self.finished {
                    return Ok(Vec::new());
                }
                let Some(position) = self.calls.iter().position(|call| call.canonical == *index)
                else {
                    return Err(CodecError::Encode {
                        protocol: PROTOCOL,
                        reason: "a tool call ended before it started".to_owned(),
                    });
                };
                let call = self.calls.remove(position);
                let value: serde_json::Value =
                    serde_json::from_str(&call.arguments).map_err(|_| CodecError::Encode {
                        protocol: PROTOCOL,
                        reason: "a tool call's arguments do not concatenate into a JSON document"
                            .to_owned(),
                    })?;
                let arguments = RawJson::new(value);
                if !arguments.is_object() {
                    return Err(CodecError::Encode {
                        protocol: PROTOCOL,
                        reason: "a tool call's arguments are not a JSON object".to_owned(),
                    });
                }
                self.content_chunk(vec![
                    parts::PartOut::Call {
                        id: &call.id,
                        name: &call.name,
                        arguments: &arguments,
                    }
                    .into(),
                ])
            }
            StreamEvent::Usage(counts) => match usage::encode(counts) {
                Some(wire) => self.chunk(&ChunkWire {
                    candidates: None,
                    usage: Some(wire),
                }),
                None => Ok(Vec::new()),
            },
            StreamEvent::Finish(reason) => {
                self.require_started()?;
                if self.finished {
                    return Ok(Vec::new());
                }
                self.finished = true;
                self.chunk(&ChunkWire {
                    candidates: Some(vec![CandidateWire {
                        content: None,
                        finish_reason: Some(response::finish_reason(*reason)),
                    }]),
                    usage: None,
                })
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
        if let Some(call) = self.calls.first() {
            return Err(CodecError::Encode {
                protocol: PROTOCOL,
                reason: format!(
                    "the stream ended with the tool call of `{}` still open, which the canonical \
                     stream never does",
                    call.name
                ),
            });
        }
        // This protocol has no terminator: the body ending is the end of the answer.
        Ok(Vec::new())
    }
}

/// The document of the chunk that opens the stream: the answer's identity.
#[derive(Serialize)]
struct IdentityWire<'a> {
    #[serde(rename = "responseId")]
    response_id: &'a str,
    #[serde(rename = "modelVersion")]
    model_version: &'a str,
}

/// One chunk of the stream: content, a finish reason, usage, or a combination.
#[derive(Serialize)]
struct ChunkWire<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    candidates: Option<Vec<CandidateWire<'a>>>,
    #[serde(rename = "usageMetadata", skip_serializing_if = "Option::is_none")]
    usage: Option<usage::UsageWire>,
}

/// One candidate of a chunk.
#[derive(Serialize)]
struct CandidateWire<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<parts::ContentOut<'a>>,
    #[serde(rename = "finishReason", skip_serializing_if = "Option::is_none")]
    finish_reason: Option<&'static str>,
}
