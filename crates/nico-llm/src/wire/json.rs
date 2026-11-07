//! Reading a JSON document, and refusing one, without ever quoting its payload.
//!
//! Every adapter here parses a buffered body the same way: the parser reads it, a failure is reported
//! as its category and position, and a modelled object refuses a member it does not declare through
//! the one rule [`diagnostic`](crate::diagnostic) owns. Five copies of that would drift; this module is
//! the single copy, and each caller maps its two refusals into its own error type.

use std::{collections::BTreeMap, fmt};

use serde::de::DeserializeOwned;

use crate::{diagnostic, schema::RawJson};

/// Parses one JSON document, reporting only the error class and the position.
///
/// `what` names the document in the sentence (`"the response body"`), because only the caller knows
/// which document it handed over. The parser's own message can quote the offending value, and every
/// body here is either the caller's prompt or the model's answer: the diagnostic says *where* the
/// document broke, never *what* it held.
///
/// # Errors
/// Returns [`JsonError`] when the bytes are not one JSON value of type `T`.
pub(crate) fn parse<T: DeserializeOwned>(body: &[u8], what: &str) -> Result<T, JsonError> {
    serde_json::from_slice(body).map_err(|error| JsonError::new(what, &error))
}

/// Names the class of a parse or serialization failure.
pub(crate) fn error_class(error: &serde_json::Error) -> &'static str {
    match error.classify() {
        serde_json::error::Category::Io => "i/o",
        serde_json::error::Category::Syntax => "syntax",
        serde_json::error::Category::Data => "data",
        serde_json::error::Category::Eof => "end of input",
    }
}

/// Why a document could not be read as the JSON value a shape expects.
///
/// The payload is deliberately absent: this value only ever carries the document's name, the failure's
/// category and its position, all of which are safe to log and to render.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{what} is not a valid document ({class} error at line {line}, column {column})")]
pub(crate) struct JsonError {
    what: String,
    class: &'static str,
    line: usize,
    column: usize,
}

impl JsonError {
    /// Builds the refusal from the position the parser stopped at.
    fn new(what: &str, error: &serde_json::Error) -> Self {
        Self {
            what: what.to_owned(),
            class: error_class(error),
            line: error.line(),
            column: error.column(),
        }
    }
}

/// Refuses the first member of an object whose shape the caller fully types.
///
/// `extra` is the flattened remainder a modelled object kept, and `path` is the document position it
/// stands at. The key is the caller's, so it is quoted only when [`diagnostic::quoted`] allows it; a
/// member whose name cannot be printed is still refused, just not named.
///
/// # Errors
/// Returns [`UnknownField`] naming the member, or an unnamed one when its name cannot be printed.
pub(crate) fn reject_extra(
    extra: &BTreeMap<String, RawJson>,
    path: &str,
) -> Result<(), UnknownField> {
    let Some(key) = extra.keys().next() else {
        return Ok(());
    };
    Err(
        match diagnostic::quoted(key, diagnostic::MAX_QUOTED_FIELD_BYTES) {
            Some(key) => UnknownField {
                reason: format!("field `{path}.{key}` is not part of this document"),
            },
            None => UnknownField {
                reason: format!("`{path}` carries a field this document does not model"),
            },
        },
    )
}

/// A member a modelled object does not declare.
///
/// It holds the finished sentence, filtered by [`diagnostic`](crate::diagnostic), so a caller maps it
/// into its own refusal — a client codec's `InvalidRequest`, an adapter's `Malformed` — without
/// re-deriving the quoting rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnknownField {
    reason: String,
}

impl UnknownField {
    /// The refusal sentence, ready for a diagnostic.
    pub(crate) fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for UnknownField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::{Value, json};

    use super::{parse, reject_extra};
    use crate::schema::RawJson;

    /// A parse failure says where the document broke and never what it held.
    #[test]
    fn a_parse_failure_names_the_position_and_not_the_payload() {
        let error = parse::<Value>(br#"{"prompt":"MARKER","next": }"#, "the body")
            .expect_err("a missing value is not a document");
        let rendered = error.to_string();
        assert!(
            rendered
                .starts_with("the body is not a valid document (syntax error at line 1, column "),
            "{rendered}"
        );
        assert!(
            !rendered.contains("MARKER"),
            "the payload must not be quoted: {rendered}"
        );
    }

    #[test]
    fn an_empty_body_is_an_end_of_input_failure() {
        let error = parse::<Value>(b"", "the body").expect_err("an empty body is not a document");
        assert!(
            error.to_string().contains("end of input error at line 1"),
            "{error}"
        );
    }

    /// A document that parses but is the wrong shape is a `data` failure, not a syntax one.
    #[test]
    fn a_shape_mismatch_is_a_data_failure() {
        let error =
            parse::<Vec<u8>>(br#"{"a":1}"#, "the body").expect_err("an object is not a list");
        assert!(error.to_string().contains("data error at line"), "{error}");
    }

    #[test]
    fn a_member_the_shape_declares_is_accepted() {
        assert_eq!(reject_extra(&BTreeMap::new(), "messages[0]"), Ok(()));
    }

    #[test]
    fn a_printed_member_is_quoted() {
        let extra = BTreeMap::from([("nickname".to_owned(), RawJson::new(json!("bob")))]);
        let error = reject_extra(&extra, "messages[0]").expect_err("an undeclared member");
        assert_eq!(
            error.reason(),
            "field `messages[0].nickname` is not part of this document"
        );
    }

    /// A member name can carry a newline or kilobytes of text: it must not reach the sentence.
    #[test]
    fn an_unprintable_member_is_refused_without_being_quoted() {
        let extra = BTreeMap::from([("evil\nkey".to_owned(), RawJson::new(json!(true)))]);
        let error = reject_extra(&extra, "messages[0]").expect_err("an undeclared member");
        assert_eq!(
            error.reason(),
            "`messages[0]` carries a field this document does not model"
        );
    }
}
