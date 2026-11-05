//! Compiled rejection presentations: the status, headers and body a policy refuses with.
//!
//! A [`Rejection`] is **data**, not a second error mapper. It is compiled once — when a document is
//! published — and rendered on the request path without deciding anything, so the only place that
//! decides `(status, code)` for a built-in failure stays [`HttpError::respond`](super::HttpError).
//! A filter that wants its refusal to look different supplies one of these instead of writing a
//! response: `proxy::failure::write_respond` is still the one writer, and it still writes a closed
//! set of headers.
//!
//! Three invariants live in the constructors, so they hold for every rejection a request can see:
//!
//! - **Status is a 4xx.** A rejection is the caller's fault. A 5xx would report the caller's mistake
//!   as a gateway failure, and a 2xx would not be a rejection at all.
//! - **The body is a template, not a format string.** Its variable parts are [`Part`] values, so an
//!   unknown variable cannot be expressed — not even by a configuration mistake. The crate that
//!   parses an operator's template is therefore the only place that can reject one.
//! - **Values are escaped by the renderer, syntax is escaped by the caller.** For a JSON template the
//!   caller writes pre-escaped JSON syntax as [`Part::Literal`] and the renderer escapes every
//!   variable it substitutes, because those values come from the request (the trace id) or from a
//!   claim (the message). `nico-http` therefore needs no JSON library; the one escaper it owns is
//!   exported as [`escape_json_string`](super::escape_json_string) so the layer that builds a JSON
//!   template from configuration does not grow a second one.

use bytes::Bytes;
use nico_core::ErrorCategory;
use pingora::http::StatusCode;

use super::push_json_string;

/// Largest accepted literal text in one template, in bytes.
///
/// The literal parts are compiled from configuration, so the bound is the point where a document
/// cannot make every rejection allocate before it can be written.
pub const MAX_LITERAL_BYTES: usize = 4 * 1024;

/// Why a rejection presentation was refused while it was compiled.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum RejectionError {
    /// The status is not a 4xx, so it cannot describe a rejection.
    #[error("a rejection status must be a 4xx, got {status}")]
    StatusNotClientError {
        /// The rejected status code.
        status: u16,
    },
    /// A field that becomes a header value or a body text is empty.
    #[error("rejection {field} must not be empty")]
    Empty {
        /// Which field was empty.
        field: &'static str,
    },
    /// A field that becomes a header value carries bytes a header cannot hold.
    #[error("rejection {field} must contain printable ASCII only")]
    NotPrintable {
        /// Which field carries the offending bytes.
        field: &'static str,
    },
    /// The template's literal text exceeds [`MAX_LITERAL_BYTES`].
    #[error("rejection template literal text exceeds {limit} bytes")]
    TemplateTooLarge {
        /// The bound that was crossed.
        limit: usize,
    },
}

/// How a template's variable values are rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BodyFormat {
    /// Variables are JSON-escaped; literals are already JSON syntax.
    Json,
    /// Variables are written verbatim; literals are the body text.
    Text,
}

/// One piece of a [`BodyTemplate`].
///
/// The variable set is closed on purpose: a rejection body can name its own stable code, its own
/// message and the request's correlation id, and nothing else. Upstream names, configuration paths
/// and credentials have no variant, so "the error response never echoes internal detail" is a
/// property of the type rather than a rule a reviewer has to check.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Part {
    /// Text emitted verbatim.
    ///
    /// It owns its bytes because the value comes from a configuration document, not from this
    /// crate's source: the built-in envelope is only one producer of a literal.
    ///
    /// In a [`BodyFormat::Json`] template this is JSON *syntax*: the caller writes the quotes,
    /// brackets and escapes, and the renderer substitutes no characters into it.
    Literal(Box<str>),
    /// The rejection's stable code, for example `auth_invalid`.
    Code,
    /// The rejection's caller-safe message.
    Message,
    /// The request's correlation id.
    TraceId,
}

/// A compiled rejection body: literal text with whitelisted variables between the pieces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyTemplate {
    format: BodyFormat,
    parts: Vec<Part>,
}

impl BodyTemplate {
    /// Compiles a body from its format and parts.
    ///
    /// # Errors
    /// Returns [`RejectionError::TemplateTooLarge`] when the literal parts together exceed
    /// [`MAX_LITERAL_BYTES`].
    pub fn new(format: BodyFormat, parts: Vec<Part>) -> Result<Self, RejectionError> {
        let literals: usize = parts
            .iter()
            .map(|part| match part {
                Part::Literal(text) => text.len(),
                Part::Code | Part::Message | Part::TraceId => 0,
            })
            .sum();
        if literals > MAX_LITERAL_BYTES {
            return Err(RejectionError::TemplateTooLarge {
                limit: MAX_LITERAL_BYTES,
            });
        }
        Ok(Self { format, parts })
    }

    /// Returns the built-in body, `{"error":{"code","message","trace_id"}}`.
    ///
    /// This is the shape a rejection renders when its plugin configures no body, so the default and a
    /// configured body travel the same path: there is no second renderer for the built-in case.
    #[must_use]
    pub fn error_envelope() -> Self {
        Self {
            format: BodyFormat::Json,
            parts: vec![
                Part::Literal(r#"{"error":{"code":""#.into()),
                Part::Code,
                Part::Literal(r#"","message":""#.into()),
                Part::Message,
                Part::Literal(r#"","trace_id":""#.into()),
                Part::TraceId,
                Part::Literal(r#""}}"#.into()),
            ],
        }
    }

    /// Returns how the variable values are rendered.
    #[must_use]
    pub const fn format(&self) -> BodyFormat {
        self.format
    }

    /// Returns the parts, in render order.
    #[must_use]
    pub fn parts(&self) -> &[Part] {
        &self.parts
    }

    /// Renders the body for one rejection and one request.
    ///
    /// `code` and `message` are the rejection's own constants; `trace_id` is caller-supplied, so in a
    /// JSON template it is escaped.
    #[must_use]
    pub fn render(&self, code: &str, message: &str, trace_id: &str) -> Bytes {
        let mut body = String::with_capacity(96 + trace_id.len());
        for part in &self.parts {
            match part {
                Part::Literal(text) => body.push_str(text),
                Part::Code => self.push_value(&mut body, code),
                Part::Message => self.push_value(&mut body, message),
                Part::TraceId => self.push_value(&mut body, trace_id),
            }
        }
        Bytes::from(body)
    }

    /// Writes one variable value, escaped when the body is JSON.
    fn push_value(&self, body: &mut String, value: &str) {
        match self.format {
            BodyFormat::Json => push_json_string(body, value),
            BodyFormat::Text => body.push_str(value),
        }
    }
}

/// One compiled refusal: what a rejecting filter wants the caller to see.
///
/// Fields are private and every constructor validates, so a `Rejection` that exists is one the data
/// plane can write: a 4xx status, printable header values and a bounded template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    /// Owned because a document configures its own content type: only the code, the message and the
    /// challenge are this crate's constants.
    content_type: Box<str>,
    challenge: Option<&'static str>,
    category: ErrorCategory,
    body: BodyTemplate,
}

impl Rejection {
    /// Compiles one rejection.
    ///
    /// `category` is carried rather than derived from the status: an authentication failure is
    /// [`ErrorCategory::Client`] and a rate limit is [`ErrorCategory::Limit`], and a caller must read
    /// that from the plugin's kind, never guess it from a number a document may have overridden.
    ///
    /// `content_type` is taken by value because it is configuration, not one of this crate's
    /// constants; `code` and `message` stay `&'static str` because they are.
    ///
    /// # Errors
    /// - [`RejectionError::StatusNotClientError`] when `status` is not a 4xx.
    /// - [`RejectionError::Empty`] when `code`, `message` or `content_type` is empty.
    /// - [`RejectionError::NotPrintable`] when a header value carries bytes a header cannot hold.
    pub fn new(
        status: StatusCode,
        code: &'static str,
        message: &'static str,
        content_type: impl Into<Box<str>>,
        category: ErrorCategory,
        body: BodyTemplate,
    ) -> Result<Self, RejectionError> {
        if !status.is_client_error() {
            return Err(RejectionError::StatusNotClientError {
                status: status.as_u16(),
            });
        }
        require_printable("code", code)?;
        require_non_empty("message", message)?;
        let content_type = content_type.into();
        require_printable("content_type", &content_type)?;
        Ok(Self {
            status,
            code,
            message,
            content_type,
            challenge: None,
            category,
            body,
        })
    }

    /// Compiles one rejection from the status a configuration document wrote.
    ///
    /// A document names a status as an integer, so the layer that compiles its presentation must not
    /// need the HTTP framework to build one. An unparseable status and a parsed one that is not a
    /// 4xx are the same answer — [`RejectionError::StatusNotClientError`] — because neither describes
    /// a rejection.
    ///
    /// # Errors
    /// The same failures as [`Rejection::new`], plus [`RejectionError::StatusNotClientError`] when
    /// `status` is not a 4xx status code at all.
    pub fn from_u16(
        status: u16,
        code: &'static str,
        message: &'static str,
        content_type: impl Into<Box<str>>,
        category: ErrorCategory,
        body: BodyTemplate,
    ) -> Result<Self, RejectionError> {
        let Ok(status) = StatusCode::from_u16(status) else {
            return Err(RejectionError::StatusNotClientError { status });
        };
        Self::new(status, code, message, content_type, category, body)
    }

    /// Adds the `www-authenticate` challenge a 401 answers with.
    ///
    /// It is a separate step because only an authentication plugin has one, and a rate limit that
    /// carried a challenge would tell the caller to authenticate against a refusal that has nothing
    /// to do with credentials.
    ///
    /// # Errors
    /// Returns [`RejectionError::Empty`] or [`RejectionError::NotPrintable`] when `challenge` cannot be
    /// a header value.
    pub fn with_challenge(mut self, challenge: &'static str) -> Result<Self, RejectionError> {
        require_printable("challenge", challenge)?;
        self.challenge = Some(challenge);
        Ok(self)
    }

    /// Returns the status to send.
    #[must_use]
    pub const fn status(&self) -> StatusCode {
        self.status
    }

    /// Returns the stable snake_case code, sent both in the body and in `x-nico-error-code`.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.code
    }

    /// Returns the caller-safe message, a fixed sentence with no internal detail.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        self.message
    }

    /// Returns the content type the body is written with.
    ///
    /// A document may configure this, so it is borrowed from the rejection rather than `'static`.
    #[must_use]
    pub fn content_type(&self) -> &str {
        &self.content_type
    }

    /// Returns the `www-authenticate` challenge, when this rejection answers an authentication
    /// failure.
    #[must_use]
    pub const fn challenge(&self) -> Option<&'static str> {
        self.challenge
    }

    /// Returns the category the failure is logged with.
    #[must_use]
    pub const fn category(&self) -> ErrorCategory {
        self.category
    }

    /// Returns the compiled body.
    #[must_use]
    pub const fn body(&self) -> &BodyTemplate {
        &self.body
    }

    /// Renders this rejection's body for one request.
    #[must_use]
    pub fn render(&self, trace_id: &str) -> Bytes {
        self.body.render(self.code, self.message, trace_id)
    }
}

/// Rejects an empty field that must carry text.
fn require_non_empty(field: &'static str, value: &str) -> Result<(), RejectionError> {
    if value.is_empty() {
        return Err(RejectionError::Empty { field });
    }
    Ok(())
}

/// Rejects a header value that is empty, or that carries anything but printable ASCII.
///
/// A header value cannot hold a control character, and a rejection is written with a fixed header set
/// — so a document-supplied value is checked here rather than failing to write later, on the request
/// path, where the only remaining answer is a `500`.
fn require_printable(field: &'static str, value: &str) -> Result<(), RejectionError> {
    require_non_empty(field, value)?;
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_graphic() || byte == b' ')
    {
        return Err(RejectionError::NotPrintable { field });
    }
    Ok(())
}

// Compiled-in proof that a compiled rejection can be shared across requests and across `.await`.
const _: fn() = || {
    const fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<Rejection>();
};

#[cfg(test)]
mod tests;
