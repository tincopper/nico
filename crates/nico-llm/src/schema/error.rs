//! The canonical error value and the validation error every shape returns.
//!
//! [`SchemaError`] is an internal contract failure: it names the field that broke an invariant and
//! is never written to a client. [`CanonicalError`] is the opposite — it is the *safe* error a
//! provider adapter produces and a client codec renders, so it carries a stable code, an HTTP
//! status and nothing that could leak a provider body or a credential.

use std::fmt;

use nico_core::ErrorCategory;
use serde::{Deserialize, Serialize};

/// Longest stable error code accepted, in bytes.
pub const MAX_CANONICAL_CODE_BYTES: usize = 64;
/// Longest caller-safe message accepted, in bytes.
pub const MAX_CANONICAL_MESSAGE_BYTES: usize = 256;
/// Longest provider error code accepted, in bytes.
///
/// Shared with the adapters that decide whether to attach a provider's own code: the bound the
/// constructor enforces and the bound a reader screens with are one number, not two.
pub(crate) const MAX_PROVIDER_CODE_BYTES: usize = 128;
/// Longest request text a diagnostic error echoes, in bytes.
const MAX_EXCERPT_BYTES: usize = 64;
/// Longest wire token accepted, in bytes.
///
/// A model name, a tool name and a tool-call id are the same kind of value: a short identifier the
/// caller or the provider chooses, retained for the life of a request, recorded in a log field and
/// sometimes put in a URL. They share one number because they share one failure mode — a caller-written
/// megabyte that costs memory and log volume while addressing nothing — and one bound is what stops
/// the three from drifting apart.
pub const MAX_TOKEN_BYTES: usize = 256;
/// Inclusive range of HTTP statuses a canonical error may carry.
const ERROR_STATUS_RANGE: std::ops::RangeInclusive<u16> = 400..=599;

/// Describes why a canonical value could not be built.
///
/// The variant names the failed invariant and the `field` names where it lives, using the dotted
/// path a reader can map back to the model (`response_format.json_schema`, `tool.name`). Request and
/// configuration text is carried only when it helps diagnose the failure, and only as a bounded
/// excerpt, so a rejected data URL or header-sized string cannot put a payload into a log line. No
/// variant carries a credential or a provider body.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SchemaError {
    /// A required string or collection is empty.
    #[error("`{field}` must not be empty")]
    Empty {
        /// Dotted path of the empty field.
        field: &'static str,
    },
    /// A string exceeds the bound its field accepts.
    #[error("`{field}` must be at most {limit} bytes, got {actual}")]
    TooLong {
        /// Dotted path of the oversized field.
        field: &'static str,
        /// Largest accepted length, in bytes.
        limit: usize,
        /// Actual length, in bytes.
        actual: usize,
    },
    /// A string is not one of the values the field accepts.
    #[error("`{field}` is not a valid value: {value}")]
    Invalid {
        /// Dotted path of the invalid field.
        field: &'static str,
        /// An excerpt of the rejected value, never the whole payload.
        value: String,
    },
    /// A float field is NaN or infinite.
    #[error("`{field}` must be a finite number")]
    NotFinite {
        /// Dotted path of the float field.
        field: &'static str,
    },
    /// A numeric field falls outside its accepted range.
    #[error("`{field}` is out of range: {value}")]
    OutOfRange {
        /// Dotted path of the numeric field.
        field: &'static str,
        /// The rejected value, rendered for diagnosis.
        value: String,
    },
    /// A field that must hold a JSON object holds another JSON kind.
    #[error("`{field}` must be a JSON object")]
    NotAnObject {
        /// Dotted path of the field.
        field: &'static str,
    },
    /// A JSON object that must declare something is empty.
    #[error("`{field}` must be a non-empty JSON object")]
    EmptyObject {
        /// Dotted path of the field.
        field: &'static str,
    },
    /// Text that must be JSON does not parse.
    #[error("`{field}` is not valid JSON: {reason}")]
    InvalidJson {
        /// Dotted path of the field.
        field: &'static str,
        /// The parser's diagnostic.
        reason: String,
    },
    /// A name is declared twice in a collection that addresses entries by name.
    #[error("{kind} `{value}` is declared more than once")]
    Duplicate {
        /// What kind of entry is duplicated (`tool`, `choice index`).
        kind: &'static str,
        /// The duplicated name, as an excerpt.
        value: String,
    },
    /// A tool choice names a tool the request does not declare.
    #[error("tool_choice names `{name}`, which no declared tool provides")]
    UndeclaredTool {
        /// The missing tool name, as an excerpt.
        name: String,
    },
    /// An extension key duplicates a field the canonical model already names.
    ///
    /// Extensions are serialized next to the modelled fields; a key that shadows one would be
    /// overwritten on the way out or swallowed on the way in.
    #[error("`{key}` is a modelled field and cannot also be an extension")]
    ReservedExtensionKey {
        /// The shadowing key.
        key: String,
    },
    /// A request carries no conversation item.
    #[error("a conversation must hold at least one item")]
    EmptyConversation,
    /// A message carries no content part.
    #[error("a message must hold at least one content part")]
    EmptyContent,
    /// A response carries no choice.
    #[error("a response must hold at least one choice")]
    EmptyChoices,
    /// Two choices share an index, so a client cannot address them.
    #[error("choice index {index} is declared more than once")]
    DuplicateChoiceIndex {
        /// The duplicated index.
        index: u32,
    },
}

/// The safe, protocol-independent failure produced by a provider adapter.
///
/// A provider adapter maps a provider status and body to this value; it never forwards the body.
/// A client codec renders this value in its own error envelope. The fields are therefore chosen to
/// be enough for a client and an operator, and nothing more: `code` is a stable machine-readable
/// identifier, `message` is caller-safe prose, `http_status` is the status the gateway will answer
/// with, and `retryable` says whether another attempt could succeed before the first byte was
/// committed. The provider's own code is kept for structured logs but is not client-facing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "CanonicalErrorWire")]
pub struct CanonicalError {
    category: ErrorCategory,
    code: String,
    message: String,
    http_status: u16,
    retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_code: Option<String>,
}

#[derive(Deserialize)]
struct CanonicalErrorWire {
    category: ErrorCategory,
    code: String,
    message: String,
    http_status: u16,
    #[serde(default)]
    retryable: bool,
    #[serde(default)]
    provider_code: Option<String>,
}

impl CanonicalError {
    /// Builds a canonical error that is not retryable.
    ///
    /// # Errors
    /// Returns [`SchemaError::Empty`] or [`SchemaError::TooLong`] when any text field is empty or
    /// over its bound, [`SchemaError::Invalid`] when `code` is not lowercase ASCII with digits and
    /// underscores, and [`SchemaError::OutOfRange`] when `http_status` is outside `400..=599`.
    pub fn try_new(
        category: ErrorCategory,
        code: impl Into<String>,
        message: impl Into<String>,
        http_status: u16,
    ) -> Result<Self, SchemaError> {
        let error = Self {
            category,
            code: code.into(),
            message: message.into(),
            http_status,
            retryable: false,
            provider_code: None,
        };
        error.validate()?;
        Ok(error)
    }

    /// Marks the error as retryable before the first client byte is committed.
    #[must_use]
    pub fn with_retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    /// Builds an error from parts the gateway itself owns.
    ///
    /// [`CanonicalError::try_new`] validates because its inputs can come from a caller or a provider.
    /// This constructor is for the gateway's own failure table, whose code, message and status are
    /// compile-time constants; it exists so that mapping an internal failure into a client-facing one
    /// is total instead of fallible. The invariant is not enforced here but asserted by the tests of
    /// the mapping that uses it, which call [`CanonicalError::validate`] on every mapped failure: a
    /// wrong constant fails the test suite rather than a request.
    pub(crate) fn from_static_parts(
        category: ErrorCategory,
        code: &'static str,
        message: &'static str,
        http_status: u16,
    ) -> Self {
        Self {
            category,
            code: code.to_owned(),
            message: message.to_owned(),
            http_status,
            retryable: false,
            provider_code: None,
        }
    }

    /// Attaches the provider's own error code for structured logging.
    ///
    /// # Errors
    /// Returns [`SchemaError::Empty`] or [`SchemaError::TooLong`] when `code` is empty or over its
    /// bound.
    pub fn with_provider_code(mut self, code: impl Into<String>) -> Result<Self, SchemaError> {
        let code = code.into();
        validate_bounded(
            "canonical_error.provider_code",
            &code,
            MAX_PROVIDER_CODE_BYTES,
        )?;
        self.provider_code = Some(code);
        Ok(self)
    }

    /// Checks every field against its invariant.
    ///
    /// # Errors
    /// Returns the same variants as [`CanonicalError::try_new`].
    pub fn validate(&self) -> Result<(), SchemaError> {
        validate_bounded("canonical_error.code", &self.code, MAX_CANONICAL_CODE_BYTES)?;
        if !self
            .code
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(SchemaError::Invalid {
                field: "canonical_error.code",
                value: excerpt(&self.code),
            });
        }
        validate_bounded(
            "canonical_error.message",
            &self.message,
            MAX_CANONICAL_MESSAGE_BYTES,
        )?;
        if !ERROR_STATUS_RANGE.contains(&self.http_status) {
            return Err(SchemaError::OutOfRange {
                field: "canonical_error.http_status",
                value: self.http_status.to_string(),
            });
        }
        if let Some(code) = &self.provider_code {
            validate_bounded(
                "canonical_error.provider_code",
                code,
                MAX_PROVIDER_CODE_BYTES,
            )?;
        }
        Ok(())
    }

    /// Returns the failure category the client codec maps to its own vocabulary.
    #[must_use]
    pub const fn category(&self) -> ErrorCategory {
        self.category
    }

    /// Returns the stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Returns the caller-safe message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns the HTTP status the gateway will answer with.
    #[must_use]
    pub const fn http_status(&self) -> u16 {
        self.http_status
    }

    /// Reports whether another attempt could succeed before the first byte was committed.
    #[must_use]
    pub const fn retryable(&self) -> bool {
        self.retryable
    }

    /// Returns the provider's own error code, when it supplied one.
    #[must_use]
    pub fn provider_code(&self) -> Option<&str> {
        self.provider_code.as_deref()
    }
}

impl TryFrom<CanonicalErrorWire> for CanonicalError {
    type Error = SchemaError;

    fn try_from(wire: CanonicalErrorWire) -> Result<Self, Self::Error> {
        let mut error = Self::try_new(wire.category, wire.code, wire.message, wire.http_status)?
            .with_retryable(wire.retryable);
        if let Some(code) = wire.provider_code {
            error = error.with_provider_code(code)?;
        }
        Ok(error)
    }
}

impl fmt::Display for CanonicalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

/// Returns the bounded excerpt of `value` a diagnostic error may carry.
///
/// Errors are logged and, when they describe a caller's mistake, may be rendered to the caller, so a
/// rejection must not echo the rejected payload: an image URL can be a multi-megabyte `data:` URL and
/// a message field can be kilobytes of caller-chosen bytes. The excerpt is cut at a character
/// boundary, so it stays valid UTF-8.
pub(super) fn excerpt(value: &str) -> String {
    if value.len() <= MAX_EXCERPT_BYTES {
        return value.to_owned();
    }
    let mut end = MAX_EXCERPT_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut excerpt = value[..end].to_owned();
    excerpt.push('…');
    excerpt
}

pub(super) fn validate_bounded(
    field: &'static str,
    value: &str,
    limit: usize,
) -> Result<(), SchemaError> {
    if value.is_empty() {
        return Err(SchemaError::Empty { field });
    }
    if value.len() > limit {
        return Err(SchemaError::TooLong {
            field,
            limit,
            actual: value.len(),
        });
    }
    Ok(())
}
