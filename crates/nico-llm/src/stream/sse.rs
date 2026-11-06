//! Server-Sent Events framing: event boundaries, cross-chunk UTF-8 and the stream terminator.
//!
//! This module owns the *framing* of an event stream and nothing else. A frame's payload is opaque
//! text: which document a payload holds, and what an `event:` name means, is the protocol's
//! business. Both sides of a conversion use the same framing, so the same bytes are never parsed by
//! two slightly different splitters.
//!
//! # Rules the decoder keeps
//!
//! - **Arbitrary chunks.** A chunk boundary can fall inside a line, inside a `\r\n` pair, inside a
//!   multi-byte character, or between two events. Only complete lines are decoded, so a split
//!   character can never be mangled and a split frame is completed by the next chunk.
//! - **Every line ending.** Lines end at `\n`, `\r\n` or `\r` (the specification allows all three); a
//!   `\r` at the end of the current buffer is held back, because the next byte may complete `\r\n`.
//! - **An event is a blank line.** `data` fields are joined with `\n`, the `event` name is kept, and
//!   comment lines (`:`) and fields this framing does not use (`id`, `retry`) are ignored. A blank
//!   line that closes no field dispatches nothing: a heartbeat is not an event.
//! - **Bounded.** An event that does not end within [`MAX_EVENT_BYTES`] is refused instead of growing
//!   the buffer for as long as the peer keeps sending. The bound counts the whole event — every
//!   `data:` line it has accumulated plus the line still waiting for its terminator — so a peer
//!   cannot escape it by opening a new line. A refusal drops what the decoder buffered, so ignoring
//!   the failure cannot grow the buffer further.
//! - **No silent truncation.** A stream that ends in the middle of an event is an error, so a caller
//!   never mistakes a cut-off frame for a complete one.

// The decoder's first production consumer is an upstream adapter, which decodes an event stream; the
// framing lands together with the client encoder because both halves must agree on one splitter.
// This expectation disappears — and warns — as soon as that adapter uses the decoder.
#![expect(
    dead_code,
    reason = "the decoder half of the framing gains its first consumer in the provider step"
)]

use std::fmt;

/// Largest event this decoder buffers, in bytes.
///
/// One event is one model chunk: a megabyte is far above any protocol's delta and far below the
/// memory a stalled stream could otherwise consume. The bound covers the event's accumulated
/// payload and the line still waiting for its terminator, so neither half can grow past it.
const MAX_EVENT_BYTES: usize = 1024 * 1024;

/// The byte-order mark a stream may begin with.
const MARK: [u8; 3] = [0xef, 0xbb, 0xbf];

/// The payload a finished event stream conventionally ends with.
pub(crate) const DONE: &str = "[DONE]";

/// Why a byte stream is not a well-formed sequence of events.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum SseError {
    /// One event did not end before the bound.
    #[error("an event did not end within {limit} bytes")]
    TooLarge {
        /// Largest event this decoder accepts.
        limit: usize,
    },
    /// The stream ended in the middle of an event.
    #[error("the stream ended in the middle of an event")]
    Incomplete,
    /// The bytes of a line are not UTF-8.
    #[error("an event line is not valid UTF-8")]
    NotUtf8,
}

/// One dispatched event.
#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct Event {
    /// The `event:` name, when the sender set one.
    pub(crate) name: Option<String>,
    /// The joined `data:` payload.
    pub(crate) data: String,
}

impl fmt::Debug for Event {
    /// Prints the name and the payload's length, never the payload: it carries the model's answer.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Event")
            .field("name", &self.name)
            .field("data_len", &self.data.len())
            .finish()
    }
}

impl Event {
    /// Reports whether this event is the end of the stream.
    pub(crate) fn is_done(&self) -> bool {
        self.data == DONE
    }
}

/// The field lines of the frame being assembled.
#[derive(Debug, Default)]
struct Frame {
    name: Option<String>,
    data: String,
    has_field: bool,
}

impl Frame {
    /// Returns how many payload bytes the frame has accumulated.
    fn payload_len(&self) -> usize {
        self.data.len()
    }

    /// Records one line of the frame.
    fn record(&mut self, line: &str) {
        if line.starts_with(':') {
            return;
        }
        let (name, value) = match line.split_once(':') {
            Some((name, value)) => (name, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match name {
            "data" => {
                self.has_field = true;
                self.data.push_str(value);
                self.data.push('\n');
            }
            "event" => {
                self.has_field = true;
                self.name = Some(value.to_owned());
            }
            // `id` and `retry` are part of the framing but not of any payload this gateway forwards.
            _ => {}
        }
    }

    /// Completes the frame, when it carried a field.
    fn dispatch(&mut self) -> Option<Event> {
        if !self.has_field {
            return None;
        }
        self.has_field = false;
        let mut data = std::mem::take(&mut self.data);
        if data.ends_with('\n') {
            data.pop();
        }
        Some(Event {
            name: self.name.take(),
            data,
        })
    }
}

/// A decoder for one event stream.
///
/// The value holds the bytes of the frame it has not seen the end of yet, so it is created once per
/// stream and never shared: a partial frame must die with the response that produced it.
#[derive(Debug, Default)]
pub(crate) struct Decoder {
    /// Every byte received and not yet dropped, plus the offset of the first one not yet consumed.
    ///
    /// A read offset rather than a `drain` per line: consuming a line only advances `read`, and the
    /// consumed prefix is dropped once per chunk by [`Decoder::compact`]. Draining per line would
    /// move the rest of the chunk for every line in it, which is quadratic in a chunk that carries
    /// many small events — a provider is free to send a megabyte of one-byte deltas in one write.
    buffer: Vec<u8>,
    /// Offset in `buffer` of the first byte not yet consumed as a line.
    read: usize,
    /// Whether the byte-order mark question is settled; see [`Decoder::strip_mark`].
    mark_settled: bool,
    frame: Frame,
}

impl Decoder {
    /// Creates a decoder holding no bytes.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Feeds one arbitrary chunk and returns every event it completes.
    ///
    /// # Errors
    /// Returns [`SseError::TooLarge`] when an event does not end within [`MAX_EVENT_BYTES`], and
    /// [`SseError::NotUtf8`] when a complete line is not UTF-8. Either failure discards what the
    /// decoder buffered, so a caller that ignores it cannot grow the buffer.
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Result<Vec<Event>, SseError> {
        self.buffer.extend_from_slice(chunk);
        self.strip_mark();
        let mut events = Vec::new();
        while let Some((end, width)) = line_end(&self.buffer[self.read..]) {
            let consumed = self.read + end + width;
            if end == 0 {
                // An event is a blank line; the line itself carries no field to record.
                self.read = consumed;
                if let Some(event) = self.frame.dispatch() {
                    events.push(event);
                }
                continue;
            }
            let line = match std::str::from_utf8(&self.buffer[self.read..self.read + end]) {
                Ok(line) => line,
                Err(_) => return Err(self.fail(SseError::NotUtf8)),
            };
            self.frame.record(line);
            self.read = consumed;
            if self.frame.payload_len() > MAX_EVENT_BYTES {
                return Err(self.fail(SseError::TooLarge {
                    limit: MAX_EVENT_BYTES,
                }));
            }
        }
        self.compact();
        if self.buffer.len() > MAX_EVENT_BYTES {
            return Err(self.fail(SseError::TooLarge {
                limit: MAX_EVENT_BYTES,
            }));
        }
        Ok(events)
    }

    /// Returns the bytes not yet consumed as lines.
    fn pending(&self) -> &[u8] {
        &self.buffer[self.read..]
    }

    /// Drops the bytes already consumed as lines.
    ///
    /// Once per chunk, after the line loop: one move of what is still pending, instead of one move
    /// of it per consumed line.
    fn compact(&mut self) {
        if self.read == 0 {
            return;
        }
        self.buffer.drain(..self.read);
        self.read = 0;
    }

    /// Decides once whether the stream begins with a byte-order mark, and strips it.
    ///
    /// The three bytes can be split across chunks, and a mark that is only partly stripped becomes
    /// part of the first field name — which this framing then ignores, losing the first event. The
    /// decision therefore waits until the buffer holds three bytes or provably cannot be a mark.
    fn strip_mark(&mut self) {
        if self.mark_settled {
            return;
        }
        let pending = self.pending();
        if pending.len() >= MARK.len() {
            let marked = pending.starts_with(&MARK);
            self.mark_settled = true;
            if marked {
                self.read += MARK.len();
            }
        } else if !MARK.starts_with(pending) {
            // Fewer bytes than the mark and not a prefix of it: the stream has no mark.
            self.mark_settled = true;
        }
        // Otherwise the buffer is a proper prefix of the mark and the next chunk decides.
    }

    /// Drops what the stream buffered and returns the failure the caller sees.
    fn fail(&mut self, error: SseError) -> SseError {
        self.buffer.clear();
        self.read = 0;
        self.frame = Frame::default();
        error
    }

    /// Signals the end of the stream and returns the events the final bytes complete.
    ///
    /// A single `\r` still buffered terminates the last line: it was held back only because the next
    /// byte could have completed `\r\n`.
    ///
    /// # Errors
    /// Returns [`SseError::Incomplete`] when an event was still open — its blank line never arrived —
    /// so a truncated frame is never reported as a complete one.
    pub(crate) fn finish(&mut self) -> Result<Vec<Event>, SseError> {
        let mut events = Vec::new();
        if self.pending() == b"\r" {
            self.read = self.buffer.len();
            if let Some(event) = self.frame.dispatch() {
                events.push(event);
            }
        }
        if !self.pending().is_empty() || self.frame.has_field {
            return Err(SseError::Incomplete);
        }
        Ok(events)
    }
}

/// Encodes one event whose payload is `data`, one `data:` field per line.
#[must_use]
pub(crate) fn encode_event(data: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    write_event(&mut bytes, data);
    bytes
}

/// Appends one event to `out`, one `data:` field per line of `data`.
pub(crate) fn write_event(out: &mut Vec<u8>, data: &str) {
    for line in data.split('\n') {
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(line.as_bytes());
        out.push(b'\n');
    }
    out.push(b'\n');
}

/// Encodes the terminator a finished stream ends with.
#[must_use]
pub(crate) fn encode_done() -> Vec<u8> {
    encode_event(DONE)
}

/// Returns where the first complete line ends and how wide its terminator is.
///
/// A `\r` at the very end of the buffer is not a line end yet: the next byte may be its `\n`.
fn line_end(buffer: &[u8]) -> Option<(usize, usize)> {
    for (index, byte) in buffer.iter().enumerate() {
        match byte {
            b'\n' => return Some((index, 1)),
            b'\r' => {
                if index + 1 == buffer.len() {
                    return None;
                }
                return Some((index, if buffer[index + 1] == b'\n' { 2 } else { 1 }));
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests;
