//! The request body a buffered route holds back until the stream ends.
//!
//! One allocation that grows with the body, bounded by the route's `max_body_bytes` (checked on every
//! push), and dropped with the request.

use std::fmt;

/// The bytes accumulated for one buffered request.
///
/// A newtype so its `Debug` reports a size instead of the bytes: a request log or a test failure must
/// never print a caller's payload.
#[derive(Default)]
pub(crate) struct BodyBuffer {
    bytes: Vec<u8>,
}

impl BodyBuffer {
    /// Appends one received chunk.
    pub(crate) fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
    }

    /// Takes the whole body, leaving the buffer empty.
    pub(crate) fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.bytes)
    }
}

impl fmt::Debug for BodyBuffer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BodyBuffer")
            .field("bytes", &self.bytes.len())
            .finish()
    }
}
