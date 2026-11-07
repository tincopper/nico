//! Encoding canonical stream events as Responses stream events.
//!
//! This protocol streams *about a response object*: every event names the output item and the position
//! it belongs to, each event carries a `sequence_number`, and the answer is complete only when
//! `response.completed` arrives carrying the whole `output` array. The encoder therefore keeps the
//! response's facts and accumulates what it has emitted, so that the completion event can repeat the
//! answer as a document. That state is per response — which is why the codec itself stays stateless.
//!
//! # What is not streamed
//!
//! - A reasoning summary is accumulated and reported in the completion event, without incremental
//!   events of its own: the canonical stream carries the text but none of the handles that make a
//!   reasoning item replayable, and this codec does not declare that capability.
//! - `finish()` writes nothing when no finish event arrived. This protocol's terminator *is*
//!   `response.completed`, which claims the answer is complete: a body that ended without one must not
//!   be dressed up as a finished answer, so the stream simply stops.
//! - A failing stream ([`StreamEvent::Error`]) writes the error event and stops.

use serde::Serialize;

use super::{PROTOCOL, error, items, response, usage};
use crate::{
    protocol::{ClientStreamEncoder, CodecError},
    schema::{FinishReason, StreamEvent, Usage},
};

/// One tool call the stream is accumulating.
#[derive(Debug)]
struct Call {
    /// The call's position in the canonical stream, which its fragments repeat.
    canonical: u32,
    /// The position of the item in the `output` array.
    index: usize,
    /// The item identifier the events name.
    item_id: String,
    /// The identifier the caller's answer must carry back.
    call_id: String,
    /// The tool being called.
    name: String,
    /// The argument fragments received so far.
    arguments: String,
}

/// The text item the stream has opened, if any.
#[derive(Debug)]
struct Text {
    /// The position of the item in the `output` array.
    index: usize,
    /// The item identifier the events name.
    item_id: String,
    /// The text received so far.
    text: String,
}

/// The encoder for one response stream.
#[derive(Debug, Default)]
pub(super) struct Encoder {
    id: Option<String>,
    model: Option<String>,
    sequence: u64,
    next_index: usize,
    reasoning: Option<Reasoning>,
    text: Option<Text>,
    calls: Vec<Call>,
    reported: Option<Usage>,
    status: Option<&'static str>,
    output: Option<Vec<items::OutputItem>>,
    completed: bool,
    terminated: bool,
}

/// The reasoning summary the stream is accumulating.
#[derive(Debug)]
struct Reasoning {
    /// The position of the item in the `output` array.
    index: usize,
    /// The text received so far.
    text: String,
}

impl Encoder {
    /// Creates an encoder that has not seen the stream's start event yet.
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Writes one named event and advances the sequence number.
    fn event<T: Serialize>(&mut self, kind: &'static str, body: T) -> Result<Vec<u8>, CodecError> {
        #[derive(Serialize)]
        struct Event<T> {
            #[serde(rename = "type")]
            kind: &'static str,
            sequence_number: u64,
            #[serde(flatten)]
            body: T,
        }
        let sequence_number = self.sequence;
        self.sequence = self.sequence.saturating_add(1);
        let document = serde_json::to_string(&Event {
            kind,
            sequence_number,
            body,
        })
        .map_err(|failure| CodecError::Encode {
            protocol: PROTOCOL,
            reason: failure.to_string(),
        })?;
        Ok(sse_event(kind, &document))
    }

    /// Opens the text item, if it is not open yet.
    fn open_text(&mut self) -> Result<Vec<u8>, CodecError> {
        if self.text.is_some() {
            return Ok(Vec::new());
        }
        let index = self.take_index();
        let item_id = format!("item_{index}");
        let mut bytes = self.event(
            "response.output_item.added",
            ItemAdded {
                output_index: index as u32,
                item: AddedItem::Message {
                    id: &item_id,
                    role: "assistant",
                    status: "in_progress",
                    content: Vec::new(),
                },
            },
        )?;
        bytes.extend(self.event(
            "response.content_part.added",
            PartAdded {
                item_id: &item_id,
                output_index: index as u32,
                content_index: 0,
                part: AddedPart::OutputText { text: "" },
            },
        )?);
        self.text = Some(Text {
            index,
            item_id,
            text: String::new(),
        });
        Ok(bytes)
    }

    /// Closes the text item, if it is open.
    fn close_text(&mut self) -> Result<Vec<u8>, CodecError> {
        let Some(text) = self.text.take() else {
            return Ok(Vec::new());
        };
        let mut bytes = self.event(
            "response.output_text.done",
            TextDone {
                item_id: &text.item_id,
                output_index: text.index as u32,
                content_index: 0,
                text: &text.text,
            },
        )?;
        bytes.extend(self.event(
            "response.output_item.done",
            ItemDone {
                output_index: text.index as u32,
                item: items::message_item(text.index, text.text),
            },
        )?);
        Ok(bytes)
    }

    /// Closes every open call.
    fn close_calls(&mut self) -> Result<Vec<u8>, CodecError> {
        let mut bytes = Vec::new();
        for call in std::mem::take(&mut self.calls) {
            bytes.extend(self.event(
                "response.output_item.done",
                ItemDone {
                    output_index: call.index as u32,
                    item: items::call_item(call.index, &call.call_id, &call.name, call.arguments),
                },
            )?);
        }
        Ok(bytes)
    }

    /// Writes the completion event with the whole answer.
    ///
    /// `output` is built by the caller *before* the item-done events consume the accumulated state:
    /// the completion event has to repeat the same items those events closed.
    fn complete(&mut self) -> Result<Vec<u8>, CodecError> {
        let (Some(status), Some(output)) = (self.status, self.output.take()) else {
            return Ok(Vec::new());
        };
        let (Some(id), Some(model)) = (self.id.clone(), self.model.clone()) else {
            return Ok(Vec::new());
        };
        self.completed = true;
        let mut document = response::opening(&id, &model);
        document.status = status;
        document.output = output;
        document.usage = self.reported.as_ref().and_then(usage::encode);
        self.event("response.completed", Completed { response: document })
    }

    /// Builds the `output` array from what the stream has seen.
    fn output(&self) -> Vec<items::OutputItem> {
        let mut output = Vec::new();
        if let Some(reasoning) = &self.reasoning {
            output.push(items::reasoning_item(
                reasoning.index,
                reasoning.text.clone(),
            ));
        }
        if let Some(text) = &self.text {
            output.push(items::message_item(text.index, text.text.clone()));
        }
        for call in &self.calls {
            output.push(items::call_item(
                call.index,
                &call.call_id,
                &call.name,
                call.arguments.clone(),
            ));
        }
        output
    }

    /// Reserves the next `output` position.
    fn take_index(&mut self) -> usize {
        let index = self.next_index;
        self.next_index = self.next_index.saturating_add(1);
        index
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
        if self.terminated
            || self.completed
            || (self.status.is_some() && !matches!(event, StreamEvent::Usage(_)))
        {
            return Ok(Vec::new());
        }
        match event {
            StreamEvent::Start { id, model } => {
                self.id = Some(id.as_str().to_owned());
                self.model = Some(model.as_str().to_owned());
                let mut bytes = self.event(
                    "response.created",
                    Created {
                        response: response::opening(id.as_str(), model.as_str()),
                    },
                )?;
                bytes.extend(self.event(
                    "response.in_progress",
                    Created {
                        response: response::opening(id.as_str(), model.as_str()),
                    },
                )?);
                Ok(bytes)
            }
            StreamEvent::TextDelta(text) => {
                self.require_start()?;
                let mut bytes = self.open_text()?;
                let Some(open) = self.text.as_mut() else {
                    return Err(CodecError::Encode {
                        protocol: PROTOCOL,
                        reason: "the text item was not opened".to_owned(),
                    });
                };
                open.text.push_str(text);
                let (item_id, index) = (open.item_id.clone(), open.index);
                bytes.extend(self.event(
                    "response.output_text.delta",
                    TextDelta {
                        item_id: &item_id,
                        output_index: index as u32,
                        content_index: 0,
                        delta: text,
                    },
                )?);
                Ok(bytes)
            }
            StreamEvent::ReasoningDelta(text) => {
                self.require_start()?;
                if self.reasoning.is_none() {
                    let index = self.take_index();
                    self.reasoning = Some(Reasoning {
                        index,
                        text: String::new(),
                    });
                }
                if let Some(reasoning) = self.reasoning.as_mut() {
                    reasoning.text.push_str(text);
                }
                Ok(Vec::new())
            }
            StreamEvent::ToolCallStart { id, name, index } => {
                self.require_start()?;
                let mut bytes = self.close_text()?;
                let output_index = self.take_index();
                let item_id = format!("item_{output_index}");
                bytes.extend(self.event(
                    "response.output_item.added",
                    ItemAdded {
                        output_index: output_index as u32,
                        item: AddedItem::FunctionCall {
                            id: &item_id,
                            call_id: id.as_str(),
                            name: name.as_str(),
                            arguments: "",
                            status: "in_progress",
                        },
                    },
                )?);
                self.calls.push(Call {
                    canonical: *index,
                    index: output_index,
                    item_id,
                    call_id: id.as_str().to_owned(),
                    name: name.as_str().to_owned(),
                    arguments: String::new(),
                });
                Ok(bytes)
            }
            StreamEvent::ToolCallDelta {
                arguments, index, ..
            } => {
                self.require_start()?;
                let Some(call) = self.calls.iter_mut().find(|call| call.canonical == *index) else {
                    return Err(CodecError::Encode {
                        protocol: PROTOCOL,
                        reason: "a tool-call fragment arrived before its item started".to_owned(),
                    });
                };
                call.arguments.push_str(arguments);
                let (item_id, output_index) = (call.item_id.clone(), call.index);
                self.event(
                    "response.function_call_arguments.delta",
                    ArgumentsDelta {
                        item_id: &item_id,
                        output_index: output_index as u32,
                        delta: arguments,
                    },
                )
            }
            StreamEvent::ToolCallEnd { .. } => Ok(Vec::new()),
            StreamEvent::Usage(usage) => {
                self.reported = Some(usage.clone());
                Ok(Vec::new())
            }
            StreamEvent::Finish(reason) => {
                // The items are closed now, but the completion event waits for the end of the body: a
                // provider may report usage after its finish event and that report belongs in the
                // answer document this event carries.
                self.output = Some(self.output());
                let mut bytes = self.close_text()?;
                bytes.extend(self.close_calls()?);
                self.status = Some(match reason {
                    FinishReason::Length | FinishReason::ContentFilter => "incomplete",
                    _ => "completed",
                });
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
        // A body that ended without a finish event is not a completed answer, and this protocol has no
        // terminator that does not claim completion.
        self.complete()
    }
}

/// Frames one event: this protocol names its events and repeats the name in the payload.
fn sse_event(kind: &str, document: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(document.len() + kind.len() + 16);
    bytes.extend_from_slice(b"event: ");
    bytes.extend_from_slice(kind.as_bytes());
    bytes.extend_from_slice(b"\ndata: ");
    bytes.extend_from_slice(document.as_bytes());
    bytes.extend_from_slice(b"\n\n");
    bytes
}

/// The `response.created` / `response.in_progress` event.
#[derive(Serialize)]
struct Created<'a> {
    response: response::ResponseWire<'a>,
}

/// The `response.completed` event.
#[derive(Serialize)]
struct Completed<'a> {
    response: response::ResponseWire<'a>,
}

/// The `response.output_item.added` event.
#[derive(Serialize)]
struct ItemAdded<'a> {
    output_index: u32,
    item: AddedItem<'a>,
}

/// An item a stream has just opened.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AddedItem<'a> {
    /// The assistant message, whose text arrives as deltas.
    Message {
        id: &'a str,
        role: &'static str,
        status: &'static str,
        content: Vec<AddedPart<'a>>,
    },
    /// A call, whose arguments arrive as deltas.
    FunctionCall {
        id: &'a str,
        call_id: &'a str,
        name: &'a str,
        arguments: &'static str,
        status: &'static str,
    },
}

/// A part a stream has just opened.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AddedPart<'a> {
    /// A text part, empty at open time.
    OutputText { text: &'a str },
}

/// The `response.content_part.added` event.
#[derive(Serialize)]
struct PartAdded<'a> {
    item_id: &'a str,
    output_index: u32,
    content_index: u32,
    part: AddedPart<'a>,
}

/// The `response.output_text.delta` event.
#[derive(Serialize)]
struct TextDelta<'a> {
    item_id: &'a str,
    output_index: u32,
    content_index: u32,
    delta: &'a str,
}

/// The `response.output_text.done` event.
#[derive(Serialize)]
struct TextDone<'a> {
    item_id: &'a str,
    output_index: u32,
    content_index: u32,
    text: &'a str,
}

/// The `response.function_call_arguments.delta` event.
#[derive(Serialize)]
struct ArgumentsDelta<'a> {
    item_id: &'a str,
    output_index: u32,
    delta: &'a str,
}

/// The `response.output_item.done` event.
#[derive(Serialize)]
struct ItemDone {
    output_index: u32,
    item: items::OutputItem,
}
