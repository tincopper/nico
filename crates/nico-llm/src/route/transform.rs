//! One converted response: provider bytes in, the client protocol's bytes out.
//!
//! The value is created per response by [`ConvertRoute::transform`](super::ConvertRoute), because a
//! streamed chunk may repeat facts the head carried (a response id, the model) and a value shared by
//! every request could not remember which response it is encoding.
//!
//! It owns the one decision the data plane cannot make: **whether this answer is buffered**.
//!
//! - A `2xx` answer to a request that asked for a stream is translated chunk by chunk — provider
//!   event, canonical event, client event — and never accumulated: an event stream is never held
//!   whole, so `max_response_bytes` does not apply to it.
//! - Every other answer is buffered, including a provider failure in answer to a stream request: a
//!   failure document is not an event stream, and a client that asked for SSE must still receive its
//!   protocol's error document rather than a truncated one. Such an answer is bounded by
//!   `max_response_bytes`, and its declared length is checked before anything is written.
//!
//! Once the head is written, a failure ends the stream: the status line is already downstream, so
//! no second status exists and the data plane records the request as cancelled. That is why every
//! failure here maps to an [`HttpError`] and never to a client reply.

use std::sync::Arc;

use nico_http::{
    error::HttpError,
    llm::{LlmResponseHead, ResponseTransform},
};

use crate::{
    Exchange, ExchangeError, StreamEncoder, protocol::ResponseFacts, provider::ProviderError,
};

/// The content type of a complete client answer.
const JSON: &str = "application/json";

/// The content type of a translated event stream.
const EVENT_STREAM: &str = "text/event-stream";

/// The header that carries the stable code of a refusal to a caller.
const ERROR_CODE: &str = "x-nico-error-code";

/// One response's conversion, driven by the data plane in arrival order.
pub(super) struct ConvertTransform {
    exchange: Arc<Exchange>,
    /// Whether the client asked for a streamed answer. A failure status overrides it: its document is
    /// buffered whatever the request asked for.
    streaming: bool,
    /// The ceiling a buffered answer may reach, in bytes.
    limit: u32,
    /// The provider's facts, recorded by [`ResponseTransform::head`]; `None` until then.
    facts: Option<ResponseFacts>,
    /// The client status the head declared, so `finish` can refuse a reply that disagrees with it.
    head_status: Option<u16>,
    /// A buffered answer's bytes, bounded by `limit`.
    buffer: Vec<u8>,
    /// The per-response stream driver, present only for a translated event stream.
    stream: Option<StreamEncoder>,
}

impl ConvertTransform {
    /// Builds the translator for one response.
    pub(super) const fn new(exchange: Arc<Exchange>, streaming: bool, limit: u32) -> Self {
        Self {
            exchange,
            streaming,
            limit,
            facts: None,
            head_status: None,
            buffer: Vec::new(),
            stream: None,
        }
    }

    /// Returns the provider facts once the head has been translated.
    fn facts(&self) -> Result<&ResponseFacts, HttpError> {
        self.facts.as_ref().ok_or_else(|| {
            HttpError::internal("the response body arrived before its head was translated")
        })
    }
}

impl ResponseTransform for ConvertTransform {
    fn head(
        &mut self,
        status: u16,
        headers: &[(String, String)],
    ) -> Result<LlmResponseHead, HttpError> {
        let facts = ResponseFacts::new(status, headers.to_vec());
        let (client_status, client_headers, streamed) = if facts.is_success() {
            if self.streaming {
                (
                    200,
                    vec![
                        (String::from("content-type"), String::from(EVENT_STREAM)),
                        (String::from("cache-control"), String::from("no-cache")),
                    ],
                    true,
                )
            } else {
                (
                    200,
                    vec![(String::from("content-type"), String::from(JSON))],
                    false,
                )
            }
        } else {
            // The status decides the client's failure; the provider's body is what carries its own
            // code, which the table keeps for the log only. The answer stays a document, so it is
            // buffered even when the request asked for a stream.
            let error = self.exchange.error_for_status(status);
            (
                error.http_status(),
                vec![
                    (String::from("content-type"), String::from(JSON)),
                    (String::from(ERROR_CODE), error.code().to_owned()),
                ],
                false,
            )
        };

        if !streamed {
            if let Some(declared) = declared_length(headers) {
                if declared > u64::from(self.limit) {
                    // The head is not written yet, so this is still answerable: the caller receives
                    // the gateway's ceiling rather than a truncated body. A chunked answer has no
                    // declared length and is metered in `push` instead, where the status is already
                    // downstream.
                    return Err(HttpError::UpstreamResponseTooLarge { limit: self.limit });
                }
            }
        }

        self.facts = Some(facts);
        self.head_status = Some(client_status);
        if streamed {
            self.stream = Some(self.exchange.stream());
        }
        LlmResponseHead::try_new(client_status, client_headers)
    }

    fn push(&mut self, chunk: &[u8]) -> Result<Vec<u8>, HttpError> {
        if let Some(stream) = self.stream.as_mut() {
            return stream.push(chunk).map_err(failure);
        }
        self.facts()?;
        self.buffer.extend_from_slice(chunk);
        if self.buffer.len() > self.limit as usize {
            return Err(HttpError::UpstreamResponseTooLarge { limit: self.limit });
        }
        // A buffered answer produces its bytes when the body is complete: there is nothing to write
        // for a prefix of a document.
        Ok(Vec::new())
    }

    fn finish(&mut self) -> Result<Vec<u8>, HttpError> {
        if let Some(stream) = self.stream.as_mut() {
            return stream.finish().map_err(failure);
        }
        let (Some(facts), Some(expected)) = (self.facts.as_ref(), self.head_status) else {
            return Err(HttpError::internal(
                "the response body ended before its head was translated",
            ));
        };
        let reply = self
            .exchange
            .respond(facts, &self.buffer)
            .map_err(failure)?;
        if reply.status() != expected {
            // The head is already written with `expected`; a reply that disagrees would either be a
            // second status or a body whose envelope contradicts the one the caller received. Both
            // are front-end defects, and ending the stream is the only honest answer left.
            return Err(HttpError::internal(format!(
                "the client protocol answered {} where the translated head declared {expected}",
                reply.status()
            )));
        }
        Ok(reply.into_body())
    }
}

/// Maps a failed exchange into the data plane's failure, after the head may already be written.
///
/// Only two outcomes are meaningful here: a peer that broke its own protocol is an upstream failure,
/// and everything else — a codec that cannot encode, a canonical value that skipped its constructors —
/// is this side's defect. Neither status is written when the head is already downstream; the value
/// only classifies the request record and ends the stream.
fn failure(error: ExchangeError) -> HttpError {
    let upstream = matches!(
        &error,
        ExchangeError::Provider(ProviderError::Malformed { .. } | ProviderError::Upstream { .. })
    );
    if upstream {
        HttpError::UpstreamFailed {
            source: Box::new(error),
        }
    } else {
        HttpError::Internal {
            source: Box::new(error),
        }
    }
}

/// Returns the length a provider declared for its body, when it declared a usable one.
///
/// A repeated or non-numeric value is treated as unknown rather than guessed: the body hook meters
/// the bytes anyway, and inventing a length would refuse an answer the provider might still deliver.
fn declared_length(headers: &[(String, String)]) -> Option<u64> {
    let mut values = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value);
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    value.trim().parse().ok()
}
