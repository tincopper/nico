//! The response half of one conversion: the head a client protocol answers with, and the translate
//! step that produces the body bytes under it.

use std::fmt;

use super::is_data_plane_owned;
use crate::{
    error::HttpError,
    llm::usage::TokenUsage,
    plugin::header::{is_header_name, is_header_value},
};

/// The head a converted response is written with.
///
/// The client protocol owns this value: a complete answer is a success in the caller's vocabulary, so
/// the status and the headers are the codec's decision and not the provider's. The data plane applies
/// it after clearing the provider's head, adds the two headers it owns (`content-length` is recomputed
/// by the framing layer, `x-nico-trace-id` identifies the request), and rejects a front-end that tries
/// to write either.
///
/// A head is constructed through [`LlmResponseHead::try_new`], so a value that reached the wire has
/// passed the two rules a response head must satisfy: the status is a final one, and every header is
/// a name and a value a peer can parse.
pub struct LlmResponseHead {
    status: u16,
    headers: Vec<(String, String)>,
}

impl LlmResponseHead {
    /// Builds a client response head.
    ///
    /// # Errors
    /// Returns [`HttpError::Internal`] when `status` is not a final status (`200`–`599`), when a name
    /// is not an RFC 9110 token, when a value holds a byte that is not visible ASCII, space or tab, or
    /// when a name is one the data plane frames or forwards a message with (`content-length`,
    /// `transfer-encoding`, `host` and every hop-by-hop header). Each of those is a front-end defect
    /// rather than a request the gateway could serve, so it answers `500` if it is still reachable
    /// before the response is committed.
    pub fn try_new(status: u16, headers: Vec<(String, String)>) -> Result<Self, HttpError> {
        // A final response is never informational: a `1xx` is forwarded untouched before a transform
        // is created, and assigning one here would write an interim head with no answer behind it.
        if !(200..=599).contains(&status) {
            return Err(HttpError::internal(format!(
                "a client response head cannot carry status {status}"
            )));
        }
        for (name, value) in &headers {
            if !is_header_name(name) {
                return Err(HttpError::internal(format!(
                    "`{name}` is not an RFC 9110 header name"
                )));
            }
            if !is_header_value(value) {
                return Err(HttpError::internal(format!(
                    "the value of `{name}` is not visible ASCII, space or tab"
                )));
            }
            if is_data_plane_owned(name) {
                return Err(HttpError::internal(format!(
                    "`{name}` describes the message framing or the connection, which the data plane owns"
                )));
            }
        }
        Ok(Self { status, headers })
    }

    /// Returns the status the caller receives.
    #[must_use]
    pub const fn status(&self) -> u16 {
        self.status
    }

    /// Returns the headers to write, in the order the front-end produced them.
    #[must_use]
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }
}

impl fmt::Debug for LlmResponseHead {
    /// Prints the status and the header names: a header value may carry a diagnostic the front-end
    /// wrote, and none of it belongs in a log line.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        formatter
            .debug_struct("LlmResponseHead")
            .field("status", &self.status)
            .field("headers", &names)
            .finish()
    }
}

/// One response's translator: provider bytes in, client bytes out.
///
/// Created by [`ConvertingRoute::transform`](super::ConvertingRoute::transform) once per response, because a chunk
/// may repeat facts the head carried (a response id, the model name) and a value shared by every
/// request could not remember which response it is encoding. The data plane drives it in arrival
/// order and writes what it returns, so the byte boundaries it sees are the provider's own.
///
/// The implementation owns two decisions the data plane cannot make:
///
/// - **Whether the body is buffered.** A non-streaming answer is decoded whole and encoded once; an
///   event stream is translated chunk by chunk. `max_response_bytes` is the ceiling the route and the
///   document agree on, so a buffered answer that would exceed it is refused instead of growing.
/// - **How a failure is presented.** A provider failure status is decoded into the client protocol's
///   error envelope by the same transform, which is why [`head`](Self::head) receives the provider's
///   status rather than only a success.
///
/// Nothing is retried after the first returned byte: a failure ends the stream, and the data plane
/// records the request as cancelled because the status line is already downstream.
pub trait ResponseTransform: Send {
    /// Translates the provider's head into the client's.
    ///
    /// `headers` is every provider header whose value is UTF-8, in arrival order; the provider's
    /// framing headers are included, so an implementation can tell a declared length from a chunked
    /// body. The returned head replaces the provider's entirely.
    ///
    /// # Errors
    /// Returns the failure that replaces the response. Before the head is written this becomes the
    /// client's error response; after it, the stream ends and no second status is written.
    fn head(
        &mut self,
        status: u16,
        headers: &[(String, String)],
    ) -> Result<LlmResponseHead, HttpError>;

    /// Translates one provider chunk into the bytes to write, in order.
    ///
    /// An empty return value means this chunk produced nothing yet: the data plane writes nothing for
    /// it and does not treat that as the end of the body, which is what a protocol needs when an event
    /// spans chunks.
    ///
    /// # Errors
    /// Returns the failure that ends the stream.
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<u8>, HttpError>;

    /// Returns the bytes that complete the body.
    ///
    /// Called at the end of the provider body, including an early but delivered end of stream. This
    /// is where a terminator (`[DONE]`, a closing event) is written, so a client whose provider ended
    /// the stream cleanly still receives a complete one. A transport failure never reaches here: the
    /// framework reports it as a failed task and the stream ends where it stood, which is why the
    /// request record — not this method — is what an operator reads for that case.
    ///
    /// # Errors
    /// Returns the failure that ends the stream.
    fn finish(&mut self) -> Result<Vec<u8>, HttpError>;

    /// Returns the token usage the exchange reported, once the body is complete.
    ///
    /// `None` has two meanings and the caller must not tell them apart: the body has not ended yet, or
    /// the provider reported nothing. Both say "there is no number here", and a consumer that wrote a
    /// zero for either would pollute every average computed downstream.
    fn usage(&self) -> Option<&TokenUsage>;
}
