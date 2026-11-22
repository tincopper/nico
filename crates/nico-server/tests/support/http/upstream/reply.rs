//! What one accepted connection answers.
//!
//! The variants are the behaviours the proxy scenarios need to observe: a complete body, an arbitrary
//! status and header set, a declared length that exceeds the bytes sent, a chunked stream written at a
//! measurable rate, and the stalls that observe cancellation and timeouts. Keeping them apart from the
//! server loop means "what the origin does" is one reviewable table.

use std::time::Duration;

/// How one accepted connection answers.
#[derive(Debug, Clone)]
pub(crate) enum Reply {
    /// Reads the request body and echoes it back with a `200`.
    Echo,
    /// Reads the request body, then answers with `body` written in `chunk`-sized pieces.
    Body {
        /// The complete response body.
        body: Vec<u8>,
        /// How many bytes one write carries.
        chunk: usize,
    },
    /// Answers with a complete response of arbitrary status and headers.
    ///
    /// `content-length` is derived from `body` unless the caller supplies one, and `connection`
    /// defaults to `close`. The scenario that translates an answer asserts on the status and the
    /// headers it configured, so this variant writes exactly those plus the framing a client needs.
    Response {
        /// The status code.
        status: u16,
        /// The headers to write, in order.
        headers: Vec<(String, String)>,
        /// The complete response body.
        body: Vec<u8>,
        /// How many bytes one write carries.
        chunk: usize,
    },
    /// Declares a body longer than the one it sends.
    ///
    /// The subject is the gateway's response ceiling: the declared length is what must be refused
    /// before the status line is written, so the bytes that follow are never reached by a client.
    Declared {
        /// The status code.
        status: u16,
        /// The length the head declares.
        content_length: usize,
        /// The bytes actually written after the head.
        body: Vec<u8>,
        /// How many bytes one write carries.
        chunk: usize,
    },
    /// Sends a chunked response with `headers`, then writes `events` this far apart, then closes.
    ///
    /// The chunked framing is what makes an event stream measurable; the headers are the caller's,
    /// so a scenario can name its own content type and the translated answer can be told apart from
    /// the origin's message.
    StreamWithHeaders {
        /// The headers to write before the framing headers.
        headers: Vec<(String, String)>,
        /// The response body pieces, in order.
        events: Vec<Vec<u8>>,
        /// Delay before each piece after the first.
        gap: Duration,
    },
    /// Sends a chunked `200` head, then writes `events` this far apart, then closes.
    ///
    /// Chunked framing is what makes an event stream measurable: Pingora 0.9.1 flushes the
    /// downstream socket after every chunk of a chunked response, but only when a
    /// `Content-Length` body is complete (see `design.md` §7). A real event stream is chunked for
    /// the same reason — it has no known end.
    Stream {
        /// The response body pieces, in order.
        events: Vec<Vec<u8>>,
        /// Delay before each piece after the first.
        gap: Duration,
    },
    /// Sends a chunked head, one chunk, and then closes without the terminating chunk.
    ///
    /// This is the "provider died mid-stream" case: the response head and a byte have reached the
    /// caller, so the response is committed and no retry may replace it. Distinct from
    /// [`Self::StallMidStream`], which holds the connection open.
    StreamThenDie {
        /// The headers to write before the framing headers.
        headers: Vec<(String, String)>,
        /// The body bytes written before the close.
        first_chunk: Vec<u8>,
    },
    /// Sends a chunked `200` head and no chunk at all, then holds the connection.
    StallAfterHead,
    /// Sends a chunked `200` head and one chunk, then holds the connection.
    StallMidStream {
        /// The body bytes sent before the stall.
        first_chunk: Vec<u8>,
    },
    /// Sends `100 Continue`, then reads the request body, then sends the final response.
    ContinueThen {
        /// The complete final response body.
        body: Vec<u8>,
    },
    /// Reads the request head, then holds the connection without answering.
    StallBeforeHead,
    /// Answers `200` with the request target the origin received — the path and query exactly as
    /// the gateway forwarded them, in original form.
    Path,
}
