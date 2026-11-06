//! The failure an LLM front-end renders in the client protocol's own error document.
//!
//! A converted LLM route answers a failed exchange in the *client's* protocol, not this data plane's
//! envelope: the codec that read the request owns the error document (a `{"error":{…}}` of its own is
//! one of them), the canonical error owns the status and the stable code, and the body already exists by
//! the time this crate sees the failure. [`LlmRejection`] is that presentation: a status, a stable
//! code, a caller-safe message, a category and the bytes the codec produced.
//!
//! The type is **data**, like a policy's [`Rejection`](super::Rejection). It is not a second mapper:
//! [`HttpError::respond`](super::HttpError::respond) turns it into the same [`Respond`](super::Respond)
//! a built-in failure produces, and the one writer still writes a closed header set. What changes is
//! only where the body comes from — a codec instead of this crate's envelope — because a client that
//! speaks the provider's own protocol must receive that protocol's failure document.
//!
//! Three invariants live in the constructor:
//!
//! - **The status is a failure.** `400`–`599`; a `2xx` here would describe a success, and a `1xx` is
//!   forwarded before any translation runs.
//! - **The code is a stable identifier.** Lowercase ASCII, digits and underscores, bounded — the same
//!   spelling rule the canonical error uses, because this value travels in `x-nico-error-code` and an
//!   operator matches alerts on it.
//! - **The message is a sentence, not a payload.** Bounded and non-empty; it reaches a log record and
//!   the [`Respond`](super::Respond) accessor, never the body (the codec already wrote that).
//!
//! The body is the codec's and is never inspected: it is a document this crate cannot parse and has
//! no reason to. Its `Debug` prints the length, so a log line can say a body exists without carrying
//! a byte of the model's or the provider's text.

use std::fmt;

use bytes::Bytes;
use nico_core::ErrorCategory;
use pingora::http::StatusCode;

/// Largest accepted stable code, in bytes.
///
/// The bound is the canonical error's, because that is where this code comes from: an exchange maps
/// its failure into `CanonicalError` first, and the front-end passes the value through. This crate
/// cannot import it — the dependency edge points the other way — so a `nico-server` test asserts the
/// two numbers still agree.
pub const MAX_REJECTION_CODE_BYTES: usize = 64;

/// Largest accepted caller-safe message, in bytes.
///
/// The canonical error's bound for the same reason; a longer sentence is a payload, not a diagnosis.
pub const MAX_REJECTION_MESSAGE_BYTES: usize = 256;

/// Why an [`LlmRejection`] was refused while it was built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LlmRejectionError {
    /// The status is outside the failure range a rejection may answer with.
    #[error("an LLM rejection status must be 400..=599, got {status}")]
    Status {
        /// The rejected status code.
        status: u16,
    },
    /// A field that becomes a header value or a log field is empty.
    #[error("llm_rejection.{field} must not be empty")]
    Empty {
        /// Which field was empty.
        field: &'static str,
    },
    /// A field exceeds its bound.
    #[error("llm_rejection.{field} must be at most {limit} bytes")]
    TooLong {
        /// Which field overran.
        field: &'static str,
        /// The bound that was crossed.
        limit: usize,
    },
    /// The code is not a stable snake_case identifier.
    #[error("llm_rejection.code must be lowercase ASCII letters, digits or underscores")]
    NotSnakeCase,
}

/// A failed LLM exchange, as the client protocol renders it.
///
/// Shared through an `Arc` inside [`HttpError::LlmRejected`](super::HttpError::LlmRejected): every
/// clone of the failure points at one presentation, and the body is never copied to reach the wire.
#[derive(Clone, PartialEq, Eq)]
pub struct LlmRejection {
    status: StatusCode,
    code: String,
    message: String,
    body: Bytes,
    category: ErrorCategory,
}

impl LlmRejection {
    /// Builds the presentation a client codec produced for one failure.
    ///
    /// `body` is the codec's own error document, already encoded in the client protocol. This crate
    /// writes it unchanged and with `application/json`: every protocol the front-end seam serves
    /// answers a failure with a JSON document, and a codec that needed another media type would have
    /// to extend this value rather than smuggle the type through a header.
    ///
    /// # Errors
    /// Returns [`LlmRejectionError::Status`] when `status` is not `400`–`599`,
    /// [`LlmRejectionError::Empty`] when `code` or `message` is empty,
    /// [`LlmRejectionError::TooLong`] when either exceeds its bound, and
    /// [`LlmRejectionError::NotSnakeCase`] when `code` is not lowercase ASCII, digits and
    /// underscores.
    pub fn new(
        status: u16,
        code: impl Into<String>,
        message: impl Into<String>,
        body: Vec<u8>,
        category: ErrorCategory,
    ) -> Result<Self, LlmRejectionError> {
        if !(400..=599).contains(&status) {
            return Err(LlmRejectionError::Status { status });
        }
        let status =
            StatusCode::from_u16(status).map_err(|_| LlmRejectionError::Status { status })?;

        let code = code.into();
        if code.is_empty() {
            return Err(LlmRejectionError::Empty { field: "code" });
        }
        if code.len() > MAX_REJECTION_CODE_BYTES {
            return Err(LlmRejectionError::TooLong {
                field: "code",
                limit: MAX_REJECTION_CODE_BYTES,
            });
        }
        if !code
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(LlmRejectionError::NotSnakeCase);
        }

        let message = message.into();
        if message.is_empty() {
            return Err(LlmRejectionError::Empty { field: "message" });
        }
        if message.len() > MAX_REJECTION_MESSAGE_BYTES {
            return Err(LlmRejectionError::TooLong {
                field: "message",
                limit: MAX_REJECTION_MESSAGE_BYTES,
            });
        }

        Ok(Self {
            status,
            code,
            message,
            body: Bytes::from(body),
            category,
        })
    }

    /// Returns the status the caller receives.
    #[must_use]
    pub const fn status(&self) -> u16 {
        self.status.as_u16()
    }

    /// Returns the stable snake_case code written in `x-nico-error-code`.
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Returns the caller-safe sentence the request record may print.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns the body the client codec encoded.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// Returns how this failure is classified, for the request record.
    #[must_use]
    pub const fn category(&self) -> ErrorCategory {
        self.category
    }

    /// Returns the status in the form the writer builds a response head from.
    pub(crate) const fn status_code(&self) -> StatusCode {
        self.status
    }

    /// Returns the body as the shared bytes the writer sends, without copying it.
    pub(crate) fn body_bytes(&self) -> Bytes {
        self.body.clone()
    }
}

impl fmt::Debug for LlmRejection {
    /// Prints the presentation's identity and the body's length: the body is the client's error
    /// document, and no log line needs its bytes.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LlmRejection")
            .field("status", &self.status.as_u16())
            .field("code", &self.code)
            .field("message", &self.message)
            .field("body_len", &self.body.len())
            .field("category", &self.category)
            .finish()
    }
}
