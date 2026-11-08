//! The request target: the model, the operation and the `alt` spelling.
//!
//! This protocol carries two request facts in the target instead of the body, so the decoder parses
//! `…/models/{model}:{operation}` out of [`RequestFacts::path`] — which the data plane passes with
//! the query string — and derives the stream flag from the operation. The body is not read yet when
//! this runs, which is exactly why the protocol puts the facts here: a caller cannot smuggle a
//! different model past the route's pinned one through the body.
//!
//! # What is parsed, and what is refused
//!
//! - The operation is one of `generateContent` or `streamGenerateContent`. The dialect's other
//!   operations (`countTokens`, `embedContent`, `predict`, …) answer a different question, not a
//!   chat turn, and answering them with a generation would be a lie → bad document.
//! - `alt` is absent or `sse`: the gateway's streamed answers are always event streams, so any other
//!   spelling asks for a wire shape this codec does not produce → refused field.
//! - Any other query parameter is refused, not preserved. The canonical request has no place for a
//!   target fact, and the parameters this dialect defines beside `alt` are credentials (`key`) and
//!   quota labels — values that must never travel into a preserved bucket that provider encoders
//!   write back onto upstream bodies.
//!
//! # Escapes are not decoded
//!
//! The model name is taken literally. The adapter that writes this dialect into an upstream path
//! refuses names that need escaping, so a name that arrives percent-encoded was written by someone
//! else, and decoding `%2F` into a path separator would let one encoded byte decide which resource
//! the conversation addresses. An escaped name flows through as a literal model name and fails the
//! route's model resolution downstream — a typed failure, never a panic.

use super::{invalid, unsupported};
use crate::schema::ModelId;

/// The path segment that introduces the model and the operation.
const MODELS_SEGMENT: &str = "/models/";

/// The operation whose answer is one document.
const BUFFERED_OPERATION: &str = "generateContent";

/// The operation whose answer is an event stream.
const STREAMING_OPERATION: &str = "streamGenerateContent";

/// The facts the request target carries.
#[derive(Debug)]
pub(super) struct Target {
    /// The model the caller addressed, taken literally from the path.
    pub(super) model: ModelId,
    /// Whether the caller asked for the streaming operation.
    pub(super) stream: bool,
}

/// Parses the request target into the model, the operation and the `alt` check.
///
/// # Errors
/// Returns a bad-document error when the target does not name a model and a known operation, and a
/// refused-field error for an `alt` other than `sse` or any other query parameter.
pub(super) fn parse(path_and_query: &str) -> Result<Target, super::CodecError> {
    let (path, query) = split_query(path_and_query);
    check_query(query)?;
    let (name, operation) = split_target(path)?;
    let stream = match operation {
        BUFFERED_OPERATION => false,
        STREAMING_OPERATION => true,
        other => {
            return Err(invalid(format!(
                "the request target names an operation this codec does not read: {}",
                super::named("the request target", other)
            )));
        }
    };
    let model = ModelId::try_new(name).map_err(|error| {
        invalid(format!(
            "the request target names an unusable model: {error}"
        ))
    })?;
    Ok(Target { model, stream })
}

/// Splits the request target into its path and query at the first `?`.
fn split_query(path_and_query: &str) -> (&str, &str) {
    match path_and_query.split_once('?') {
        Some((path, query)) => (path, query),
        None => (path_and_query, ""),
    }
}

/// Checks the query: `alt` may be absent or `sse`, and nothing else may be there.
fn check_query(query: &str) -> Result<(), super::CodecError> {
    if query.is_empty() {
        return Ok(());
    }
    let mut alt_seen = false;
    for pair in query.split('&') {
        let Some((name, value)) = pair.split_once('=') else {
            return Err(invalid(
                "the request target's query carries a parameter without a value",
            ));
        };
        if name != "alt" {
            return Err(unsupported(super::named("query", name)));
        }
        if alt_seen {
            return Err(invalid("field `alt` is written more than once"));
        }
        alt_seen = true;
        if value != "sse" {
            return Err(unsupported("alt"));
        }
    }
    Ok(())
}

/// Splits the path into the model name and the operation.
fn split_target(path: &str) -> Result<(&str, &str), super::CodecError> {
    let Some(position) = path.find(MODELS_SEGMENT) else {
        return Err(invalid(
            "the request target does not name a model and an operation",
        ));
    };
    let rest = &path[position + MODELS_SEGMENT.len()..];
    let Some(colon) = rest.find(':') else {
        return Err(invalid(
            "the request target does not name an operation after the model",
        ));
    };
    // Both markers are ASCII, so every position `find` returns is a char boundary and the slices
    // below are safe for any byte sequence the rest of the target holds.
    let (name, operation) = (&rest[..colon], &rest[colon + 1..]);
    if name.is_empty() {
        return Err(invalid("the request target names an empty model"));
    }
    if name.contains('/') || operation.contains('/') {
        return Err(invalid(
            "the request target does not name a model and an operation",
        ));
    }
    Ok((name, operation))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A target both operations accept parses into the model and the stream flag.
    #[test]
    fn both_operations_parse_with_and_without_the_stream_query() {
        let buffered = parse("/v1beta/models/gemini-2.0-flash:generateContent")
            .expect("a buffered target is readable");
        assert_eq!(buffered.model.as_str(), "gemini-2.0-flash");
        assert!(!buffered.stream);

        let streamed = parse("/v1beta/models/gemini-2.0-flash:streamGenerateContent?alt=sse")
            .expect("a streamed target is readable");
        assert_eq!(streamed.model.as_str(), "gemini-2.0-flash");
        assert!(streamed.stream);
    }

    /// The prefix before `/models/` belongs to the deployment, not to the model.
    #[test]
    fn any_prefix_before_the_models_segment_is_accepted() {
        let target = parse("/proxy-prefix/v1beta/models/gemini-2.0-flash:generateContent")
            .expect("a prefixed target is readable");
        assert_eq!(target.model.as_str(), "gemini-2.0-flash");
    }

    /// An `alt` this codec cannot answer is refused by name.
    #[test]
    fn an_alt_other_than_sse_is_refused() {
        let error = parse("/v1beta/models/m:streamGenerateContent?alt=json")
            .expect_err("a non-sse answer shape is refused");
        assert!(matches!(
            error,
            super::super::CodecError::UnsupportedField { field, .. } if field == "alt"
        ));
    }

    /// A query parameter the canonical request cannot carry is refused, never dropped.
    #[test]
    fn an_unknown_query_parameter_is_refused_by_name() {
        let error = parse("/v1beta/models/m:generateContent?key=SECRET&alt=sse")
            .expect_err("a query credential must not travel");
        let super::super::CodecError::UnsupportedField { field, .. } = &error else {
            panic!("expected UnsupportedField, got {error:?}");
        };
        assert_eq!(field.as_ref(), "query.key");
        let rendered = error.to_string();
        assert!(
            !rendered.contains("SECRET"),
            "a refusal names the field, never its value: {rendered}"
        );
    }

    /// The operations this dialect reserves for other questions are bad documents here.
    #[test]
    fn another_operation_is_a_bad_document() {
        let error = parse("/v1beta/models/gemini-2.0-flash:countTokens")
            .expect_err("a non-generation operation is refused");
        assert!(matches!(
            error,
            super::super::CodecError::InvalidRequest { .. }
        ));
        // The operation is a bounded, printable path token, so the same rule that names an unknown
        // member names it; garbage that cannot be printed is refused unnamed instead.
        assert!(
            error.to_string().contains("countTokens"),
            "a printable operation is named: {error}"
        );
    }

    /// Targets that do not separate a model from an operation are bad documents.
    #[test]
    fn targets_without_a_model_or_an_operation_are_bad_documents() {
        for target in [
            "/v1beta/models",
            "/v1beta/models/gemini-2.0-flash",
            "/v1beta/models/:generateContent",
            "/v1beta/models/gemini-2.0-flash:generateContent/extra",
            "/v1/chat/completions",
        ] {
            let error = parse(target).expect_err("a target without both facts is refused");
            assert!(
                matches!(error, super::super::CodecError::InvalidRequest { .. }),
                "`{target}` is a bad document, got {error:?}"
            );
        }
    }

    /// Bytes that are not plain ASCII in the prefix or the model parse or refuse by type; they never
    /// panic. The slices are taken at ASCII markers, so any byte sequence behind them is safe.
    #[test]
    fn an_unusual_target_is_refused_or_parsed_without_panicking() {
        let escaped = parse("/v1beta/models/gemini%2Fpro:generateContent")
            .expect("an escaped name is one literal model name");
        assert_eq!(escaped.model.as_str(), "gemini%2Fpro");

        // Escapes are not decoded: a name the adapter would refuse to write flows through
        // literally and fails the route's model resolution downstream, not here.
        let encoded = parse("/v1beta/models/%FF%FE:generateContent")
            .expect("an encoded name is one literal model name");
        assert_eq!(encoded.model.as_str(), "%FF%FE");

        let multibyte = parse("/v1β/models/gemini-2.0-flash:generateContent")
            .expect("a prefix the deployment owns is not this codec's concern");
        assert_eq!(multibyte.model.as_str(), "gemini-2.0-flash");

        for target in [
            "/v1beta/models/gemini-2.0-flash:generate%Content",
            "/v1beta/models/gemini-2.0-flash:",
            "/v1beta/models/gemini-2.0-flash:generateContent?alt",
            "/v1beta/models/gemini-2.0-flash:generateContent?alt=sse&alt=sse",
        ] {
            let error = parse(target).expect_err("a target this codec cannot read is refused");
            assert!(
                matches!(error, super::super::CodecError::InvalidRequest { .. })
                    || matches!(error, super::super::CodecError::UnsupportedField { .. }),
                "`{target}` is refused by type, got {error:?}"
            );
        }
    }
}
