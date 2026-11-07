//! Folding a canonical event stream into one complete response.
//!
//! This dialect's endpoint is always the streaming operation, so a buffered answer arrives as an event
//! stream and has to become one [`ChatResponse`](crate::schema::ChatResponse). This module is that
//! fold, and only that: [`super::response::decode`] is its one caller.
//!
//! It is a state machine over the canonical events, not a second document reader: the identity comes
//! from `Start`, text and reasoning are concatenated, a tool call's argument fragments are concatenated
//! until its end closes it, and the last usage and finish reason win. A second start, an event before
//! the start, a fragment for a call that is not open, a call that ends twice or with another id, and an
//! answer that ends without a finish reason or with a call still open are all refused: each of them
//! describes an answer no single response can be.
//!
//! The fold does not abstract over anything: a second consumer of "events in, response out" is what
//! would justify promoting it to the canonical side, and today there is none. `ReasoningDelta` carries
//! text only, so the reasoning this fold produces has no signature — the gap the module documentation
//! of [`super`] records.

use std::collections::BTreeMap;

use super::{malformed, schema_failure};
use crate::{
    provider::ProviderError,
    schema::{
        AssistantTurn, ChatResponse, Choice, ContentPart, FinishReason, ModelId, RawJson,
        Reasoning, ResponseId, StreamEvent, ToolCall, ToolCallId, ToolName, Usage,
    },
};

/// Folds the events of one answer into a complete response.
///
/// # Errors
/// Returns [`ProviderError::Upstream`] when the answer carried a failure event, which is the
/// provider's own failure and is answered in the caller's protocol, and
/// [`ProviderError::Malformed`] when the events do not describe one complete response.
pub(super) fn build(events: Vec<StreamEvent>) -> Result<ChatResponse, ProviderError> {
    let mut fold = Fold::default();
    for event in events {
        fold.push(event)?;
    }
    fold.finish()
}

/// The state one answer's events accumulate into.
#[derive(Default)]
struct Fold {
    identity: Option<(ResponseId, ModelId)>,
    text: String,
    reasoning: String,
    open: BTreeMap<u32, OpenCall>,
    closed: BTreeMap<u32, ToolCall>,
    usage: Option<Usage>,
    finish: Option<FinishReason>,
}

/// A tool call whose arguments are still being concatenated.
struct OpenCall {
    id: ToolCallId,
    name: ToolName,
    arguments: String,
}

impl Fold {
    /// Records one event.
    fn push(&mut self, event: StreamEvent) -> Result<(), ProviderError> {
        match event {
            StreamEvent::Start { id, model } => {
                if self.identity.is_some() {
                    return Err(malformed(
                        "the answer announced its response more than once",
                    ));
                }
                self.identity = Some((id, model));
                Ok(())
            }
            StreamEvent::TextDelta(text) => {
                self.ready()?;
                self.text.push_str(&text);
                Ok(())
            }
            StreamEvent::ReasoningDelta(text) => {
                self.ready()?;
                self.reasoning.push_str(&text);
                Ok(())
            }
            StreamEvent::ToolCallStart { id, name, index } => {
                self.ready()?;
                if self.open.contains_key(&index) || self.closed.contains_key(&index) {
                    return Err(malformed("the answer reused a tool call index"));
                }
                self.open.insert(
                    index,
                    OpenCall {
                        id,
                        name,
                        arguments: String::new(),
                    },
                );
                Ok(())
            }
            StreamEvent::ToolCallDelta {
                id,
                arguments,
                index,
            } => {
                self.ready()?;
                let Some(call) = self.open.get_mut(&index) else {
                    return Err(malformed("a tool call delta names no open call"));
                };
                if call.id != id {
                    return Err(malformed("a tool call delta names another call"));
                }
                call.arguments.push_str(&arguments);
                Ok(())
            }
            StreamEvent::ToolCallEnd { id, index } => {
                self.ready()?;
                let Some(call) = self.open.remove(&index) else {
                    return Err(malformed("a tool call ended without having started"));
                };
                if call.id != id {
                    return Err(malformed("a tool call ended with another call's id"));
                }
                if call.arguments.is_empty() {
                    return Err(malformed("a tool call ended without arguments"));
                }
                // The fragments are the model's output; a parse failure names the field, never the
                // document, so the reason is fixed instead of the parser's message.
                let arguments = RawJson::parse(&call.arguments)
                    .map_err(|_| malformed("a tool call's arguments are not a JSON document"))?;
                self.closed.insert(
                    index,
                    ToolCall::try_new(call.id, call.name, arguments).map_err(schema_failure)?,
                );
                Ok(())
            }
            StreamEvent::Usage(usage) => {
                self.ready()?;
                self.usage = Some(usage);
                Ok(())
            }
            StreamEvent::Finish(reason) => {
                self.ready()?;
                if self.finish.is_some() {
                    return Err(malformed(
                        "the answer reported a finish reason more than once",
                    ));
                }
                self.finish = Some(reason);
                Ok(())
            }
            // The provider's own failure, already mapped: the exchange answers it in the caller's
            // protocol, so it is not folded into an answer that does not exist.
            StreamEvent::Error(error) => Err(ProviderError::Upstream { error }),
        }
    }

    /// Refuses an event that precedes the response it belongs to.
    fn ready(&self) -> Result<(), ProviderError> {
        if self.identity.is_none() {
            return Err(malformed(
                "a stream event arrived before the response it belongs to",
            ));
        }
        Ok(())
    }

    /// Builds the response the events described.
    fn finish(self) -> Result<ChatResponse, ProviderError> {
        let Some((id, model)) = self.identity else {
            return Err(malformed("the answer never announced its response"));
        };
        let Some(finish) = self.finish else {
            return Err(malformed("the answer reports no finish reason"));
        };
        if !self.open.is_empty() {
            return Err(malformed("the answer ended with a tool call still open"));
        }
        // An empty text buffer is an answer that produced no visible content (a content filter, for
        // example), not a zero-length content part: the canonical turn may be empty.
        let content = if self.text.is_empty() {
            Vec::new()
        } else {
            vec![ContentPart::text(self.text)]
        };
        let mut turn = AssistantTurn::try_new(content, self.closed.into_values().collect())
            .map_err(schema_failure)?;
        if !self.reasoning.trim().is_empty() {
            turn = turn
                .with_reasoning(Reasoning::try_new(self.reasoning, None).map_err(schema_failure)?);
        }
        let choice = Choice::try_new(0, turn, finish).map_err(schema_failure)?;
        let mut response =
            ChatResponse::try_new(id, model, vec![choice]).map_err(schema_failure)?;
        if let Some(usage) = self.usage {
            response = response.with_usage(usage);
        }
        response.validate().map_err(schema_failure)?;
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    //! The refusals an event stream from this adapter cannot produce, kept where the fold is: an
    //! integration test can only reach the fold through `decode_response`, and the decoder refuses
    //! these shapes first.

    use super::build;
    use crate::{
        provider::ProviderError,
        schema::{
            CanonicalError, FinishReason, ModelId, ResponseId, StreamEvent, ToolCallId, ToolName,
        },
    };

    /// The identity every fixture answer announces.
    fn start() -> StreamEvent {
        StreamEvent::Start {
            id: ResponseId::try_new("resp-1").expect("a valid id"),
            model: ModelId::try_new("gemini-2.0-flash").expect("a valid name"),
        }
    }

    /// The reason a fixture answer finishes for.
    fn finish() -> StreamEvent {
        StreamEvent::Finish(FinishReason::Stop)
    }

    /// A tool call's start event under one index.
    fn call_start(id: &str, index: u32) -> StreamEvent {
        StreamEvent::ToolCallStart {
            id: ToolCallId::try_new(id).expect("a valid id"),
            name: ToolName::try_new("weather").expect("a valid name"),
            index,
        }
    }

    /// Asserts that a fold is refused as a malformed answer.
    fn rejected(events: Vec<StreamEvent>) {
        let error = build(events).expect_err("the events do not describe one answer");
        assert!(
            matches!(error, ProviderError::Malformed { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn an_event_before_the_start_is_refused() {
        rejected(vec![StreamEvent::TextDelta("hi".to_owned()), finish()]);
    }

    #[test]
    fn an_answer_that_never_announces_its_response_is_refused() {
        rejected(vec![finish()]);
    }

    #[test]
    fn a_second_start_is_refused() {
        rejected(vec![start(), start(), finish()]);
    }

    #[test]
    fn an_answer_without_a_finish_reason_is_refused() {
        rejected(vec![start(), StreamEvent::TextDelta("hi".to_owned())]);
    }

    #[test]
    fn two_finish_reasons_are_refused() {
        rejected(vec![start(), finish(), finish()]);
    }

    #[test]
    fn a_delta_for_no_open_call_is_refused() {
        rejected(vec![
            start(),
            StreamEvent::ToolCallDelta {
                id: ToolCallId::try_new("call_0").expect("a valid id"),
                arguments: "{}".to_owned(),
                index: 0,
            },
            finish(),
        ]);
    }

    #[test]
    fn a_delta_for_another_call_is_refused() {
        rejected(vec![
            start(),
            call_start("call_0", 0),
            StreamEvent::ToolCallDelta {
                id: ToolCallId::try_new("call_9").expect("a valid id"),
                arguments: "{}".to_owned(),
                index: 0,
            },
            finish(),
        ]);
    }

    #[test]
    fn a_call_that_ends_with_another_id_is_refused() {
        rejected(vec![
            start(),
            call_start("call_0", 0),
            StreamEvent::ToolCallEnd {
                id: ToolCallId::try_new("call_9").expect("a valid id"),
                index: 0,
            },
            finish(),
        ]);
    }

    #[test]
    fn a_call_that_ends_without_arguments_is_refused() {
        rejected(vec![
            start(),
            call_start("call_0", 0),
            StreamEvent::ToolCallEnd {
                id: ToolCallId::try_new("call_0").expect("a valid id"),
                index: 0,
            },
            finish(),
        ]);
    }

    #[test]
    fn an_answer_that_ends_with_a_call_still_open_is_refused() {
        rejected(vec![start(), call_start("call_0", 0), finish()]);
    }

    #[test]
    fn a_failure_event_is_the_providers_failure_not_an_answer() {
        let error = build(vec![
            start(),
            StreamEvent::Error(
                CanonicalError::try_new(
                    nico_core::ErrorCategory::Upstream,
                    "upstream_error",
                    "the upstream failed",
                    502,
                )
                .expect("a valid error"),
            ),
        ])
        .expect_err("a failed answer is not folded");
        assert!(matches!(error, ProviderError::Upstream { .. }), "{error:?}");
    }
}
