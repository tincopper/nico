//! The rejection presentation one plugin entry configures.
//!
//! A rejection body is modelled as data, not as a template language: a JSON body becomes a
//! [`BodyValue`] tree and every other body is a literal [`String`]. Both are compiled into the
//! response the data plane writes, which is why nothing here parses a variable name — the crate that
//! implements the plugin owns that, and this module only guarantees the shape the document allows.
//!
//! The body's shape follows the *effective* content type, so a body written without a content type
//! is read as JSON: one default ([`DEFAULT_CONTENT_TYPE`]), one rule.
//!
//! ## Why the conversion is here
//!
//! A YAML value has spellings JSON does not (`!tag`, non-string mapping keys, `.inf`), so the
//! conversion is fallible and happens while the document is assembled. Doing it here rather than in
//! the plugin crate means the document is either fully representable when it is published, or it was
//! rejected with a field name.

use super::{DEFAULT_CONTENT_TYPE, is_json_content_type};
use crate::config::error::DocumentError;

/// The rejection presentation one plugin entry configures.
///
/// Every field is optional and falls back to the implementing crate's built-in presentation, so a
/// document that configures nothing keeps the stable default shape. Fields are private and there is
/// no public constructor: a value only exists after the document passed shape validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorResponse {
    pub(super) status: Option<u16>,
    pub(super) content_type: String,
    pub(super) body: Option<ErrorBody>,
}

impl ErrorResponse {
    /// Returns the configured status, when the entry overrides the default one.
    ///
    /// The default depends on the plugin family (an authentication failure and a rate limit do not
    /// answer the same status), which is why it is not defaulted here.
    #[must_use]
    pub const fn status(&self) -> Option<u16> {
        self.status
    }

    /// Returns the content type of the rejection body.
    ///
    /// The effective value: the entry's own content type, or `application/json` when it omits one.
    /// It follows that a body written without a content type is read as JSON.
    #[must_use]
    pub fn content_type(&self) -> &str {
        &self.content_type
    }

    /// Returns the configured body, when the entry overrides the built-in envelope.
    #[must_use]
    pub const fn body(&self) -> Option<&ErrorBody> {
        self.body.as_ref()
    }
}

/// A configured rejection body, already matching the shape its content type requires.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorBody {
    /// A structured value rendered as JSON, used for a JSON content type.
    Json(BodyValue),
    /// A literal body, used for every other content type.
    Text(String),
}

/// A JSON-representable value a rejection body may carry.
///
/// The document is untrusted, so the value a body is written as is converted into this type while
/// the document is assembled: a YAML tag, a non-string object key or a non-finite number has no JSON
/// spelling and is rejected there instead of producing an invalid body later.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BodyValue {
    /// JSON `null`.
    Null,
    /// A JSON boolean.
    Bool(bool),
    /// A JSON number, already in the spelling it is rendered with.
    Number(String),
    /// A JSON string.
    String(String),
    /// A JSON array.
    Array(Vec<BodyValue>),
    /// A JSON object, in document order.
    Object(Vec<(String, BodyValue)>),
}

/// Builds one [`ErrorResponse`], deciding the body's shape from the effective content type.
pub(super) fn build_error_response(
    raw: crate::config::raw::RawErrorResponse,
) -> Result<ErrorResponse, DocumentError> {
    let content_type = raw
        .content_type
        .unwrap_or_else(|| DEFAULT_CONTENT_TYPE.to_owned());
    let body = raw
        .body
        .map(|body| build_error_body(&content_type, &body))
        .transpose()?;
    Ok(ErrorResponse {
        status: raw.status,
        content_type,
        body,
    })
}

/// Converts a written body into the shape its content type requires.
fn build_error_body(
    content_type: &str,
    body: &serde_norway::Value,
) -> Result<ErrorBody, DocumentError> {
    if is_json_content_type(content_type) {
        return body_value(body).map(ErrorBody::Json).map_err(invalid_body);
    }
    match body {
        serde_norway::Value::String(text) => Ok(ErrorBody::Text(text.clone())),
        // A body whose content type is not JSON is a literal string: anything else would be a
        // structured value rendered as whatever the type is, which the data plane cannot represent.
        _ => Err(invalid_body(
            "must be a string unless the content type is `application/json`",
        )),
    }
}

/// Converts one YAML value into a JSON-representable value.
fn body_value(value: &serde_norway::Value) -> Result<BodyValue, &'static str> {
    match value {
        serde_norway::Value::Null => Ok(BodyValue::Null),
        serde_norway::Value::Bool(flag) => Ok(BodyValue::Bool(*flag)),
        serde_norway::Value::Number(number) => number_value(number),
        serde_norway::Value::String(text) => Ok(BodyValue::String(text.clone())),
        serde_norway::Value::Sequence(items) => items
            .iter()
            .map(body_value)
            .collect::<Result<Vec<_>, _>>()
            .map(BodyValue::Array),
        serde_norway::Value::Mapping(mapping) => mapping
            .iter()
            .map(|(key, value)| {
                let serde_norway::Value::String(key) = key else {
                    return Err("object keys must be strings");
                };
                Ok((key.clone(), body_value(value)?))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(BodyValue::Object),
        // A YAML tag has no JSON spelling: it would either be dropped (changing the body) or
        // rendered (producing a document no JSON parser accepts).
        serde_norway::Value::Tagged(_) => Err("must not use YAML tags"),
    }
}

/// Converts one YAML number into its JSON spelling.
fn number_value(number: &serde_norway::Number) -> Result<BodyValue, &'static str> {
    if let Some(value) = number.as_i64() {
        return Ok(BodyValue::Number(value.to_string()));
    }
    if let Some(value) = number.as_u64() {
        return Ok(BodyValue::Number(value.to_string()));
    }
    let Some(value) = number.as_f64() else {
        return Err("must be a JSON number");
    };
    if !value.is_finite() {
        return Err("must be a finite number");
    }
    Ok(BodyValue::Number(value.to_string()))
}

/// Builds the error for a body that cannot be expressed as its content type requires.
fn invalid_body(reason: &'static str) -> DocumentError {
    DocumentError::InvalidField {
        field: "plugins[].error_response.body",
        reason: reason.into(),
    }
}
