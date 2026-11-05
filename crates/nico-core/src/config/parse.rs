//! Bytes → bounded, decoded, shaped document.
//!
//! [`parse`] owns everything that can be decided without knowing the schema's meaning, in this
//! fixed order:
//!
//! 1. the byte bound from [`ParseLimits::max_bytes`];
//! 2. UTF-8 decoding;
//! 3. YAML anchor and alias rejection;
//! 4. YAML syntax (`serde_norway::Value`);
//! 5. node-count and nesting-depth bounds from [`SchemaBounds`];
//! 6. the serde shape ([`RawDocument`]);
//! 7. [`assemble`](super::validate::assemble), which applies the remaining validation stages.
//!
//! Node and depth counting are iterative on purpose: the document is untrusted, and a recursive
//! counter would be the very stack exhaustion the depth bound exists to prevent.

use serde::Deserialize;
use serde_norway::Value;

use super::{
    bounds::{Bound, ParseLimits, SchemaBounds},
    document::Document,
    error::DocumentError,
    raw::RawDocument,
    validate,
};

/// Bytes that end a YAML token, so an `&` or `*` right after one starts a new token.
///
/// The closing quote bytes are included because they terminate the scalar token themselves: in
/// `key: "#"&a` the quote is preceded by whitespace and the anchor starts a fresh token.
const TOKEN_SEPARATORS: &[u8] = b"[{,-:\"'";

/// Lexical state of [`reject_aliases`].
///
/// Only [`Scan::Plain`] carries live YAML indicators; the other states exist so a quote or a
/// comment cannot hide the rest of a line from the scan.
#[derive(Clone, Copy)]
enum Scan {
    /// Outside any quoted scalar or comment.
    Plain,
    /// Inside a single-quoted scalar, where `''` is the only escape.
    SingleQuoted,
    /// Inside a double-quoted scalar; `escaped` records a pending backslash.
    DoubleQuoted {
        /// Whether the previous byte was a backslash that has not yet escaped anything.
        escaped: bool,
    },
    /// Inside a comment, which runs to the end of the line.
    Comment,
}

/// Parses a document up to, but not including, reference and semantic validation.
///
/// # Errors
/// See [`Document::parse`]; this function returns the same variants in the same order.
pub(super) fn parse(bytes: &[u8], limits: ParseLimits) -> Result<Document, DocumentError> {
    if bytes.len() > limits.max_bytes() {
        return Err(DocumentError::TooLarge {
            size: bytes.len(),
            limit: limits.max_bytes(),
        });
    }

    let text = std::str::from_utf8(bytes).map_err(|_| DocumentError::NotUtf8)?;
    reject_aliases(text)?;

    let value: Value =
        serde_norway::from_str(text).map_err(|source| DocumentError::Yaml { source })?;
    measure(&value, limits.schema())?;

    let raw = RawDocument::deserialize(value).map_err(|source| DocumentError::Yaml { source })?;
    validate::assemble(raw, limits)
}

/// Rejects YAML anchors (`&name`) and aliases (`*name`).
///
/// Anchors are refused rather than expanded: one anchor can make a document inside the byte budget
/// expand into an arbitrarily large graph, and the node bound runs after expansion.
///
/// The scan is lexical and deliberately narrow. It tracks single-quoted scalars, double-quoted
/// scalars (with backslash escapes) and comments, so a `#` or `&` inside a quoted scalar is literal
/// text and a `#` in plain context only starts a comment at the start of a line or after
/// whitespace. An indicator counts only at the start of a token — at the start of the input, after
/// whitespace, after a closing quote, or after `[`, `{`, `,`, `-`, `:` — and only when followed by
/// an identifier byte, so a `*` inside a scalar (`/api/*`, `"a*b"`) passes.
///
/// Known limitation: block scalars (`|`, `>`) are not part of this schema, and their content is
/// scanned as ordinary text. An anchor or alias written inside a block scalar is therefore rejected
/// too; that is a false positive on a construct the schema does not accept anyway.
///
/// It is a guard in front of the YAML parser, not a YAML parser, so it approximates in both
/// directions. A lexical scan cannot see every construct the parser will accept, which is why this
/// check runs *before* parsing: the bounds in [`measure`] apply to the expanded tree, so they cannot
/// undo an expansion that already happened.
fn reject_aliases(text: &str) -> Result<(), DocumentError> {
    let bytes = text.as_bytes();
    let mut state = Scan::Plain;
    let mut index = 0_usize;

    while let Some(&byte) = bytes.get(index) {
        match state {
            Scan::Comment => {
                if byte == b'\n' {
                    state = Scan::Plain;
                }
            }
            Scan::SingleQuoted => {
                if byte == b'\'' {
                    if bytes.get(index + 1) == Some(&b'\'') {
                        // `''` is a literal quote; the scalar continues.
                        index += 1;
                    } else {
                        state = Scan::Plain;
                    }
                }
            }
            Scan::DoubleQuoted { escaped: true } => state = Scan::DoubleQuoted { escaped: false },
            Scan::DoubleQuoted { escaped: false } => match byte {
                b'\\' => state = Scan::DoubleQuoted { escaped: true },
                b'"' => state = Scan::Plain,
                _ => {}
            },
            Scan::Plain => match byte {
                b'\'' => state = Scan::SingleQuoted,
                b'"' => state = Scan::DoubleQuoted { escaped: false },
                b'#' if at_line_start(bytes, index) || preceded_by_space(bytes, index) => {
                    state = Scan::Comment;
                }
                b'&' | b'*'
                    if token_starts_at(bytes, index)
                        && bytes.get(index + 1).is_some_and(|next| is_name_byte(*next)) =>
                {
                    return Err(DocumentError::AliasForbidden);
                }
                _ => {}
            },
        }

        index += 1;
    }

    Ok(())
}

/// Returns whether `index` begins a YAML token.
fn token_starts_at(bytes: &[u8], index: usize) -> bool {
    match index
        .checked_sub(1)
        .and_then(|previous| bytes.get(previous))
    {
        None => true,
        Some(byte) => byte.is_ascii_whitespace() || TOKEN_SEPARATORS.contains(byte),
    }
}

/// Returns whether `index` sits at the start of a line.
fn at_line_start(bytes: &[u8], index: usize) -> bool {
    index == 0 || bytes.get(index - 1) == Some(&b'\n')
}

/// Returns whether the byte before `index` is a space or a tab.
fn preceded_by_space(bytes: &[u8], index: usize) -> bool {
    index
        .checked_sub(1)
        .and_then(|previous| bytes.get(previous))
        .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r'))
}

/// Returns whether `byte` may appear in an anchor or alias name.
const fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
}

/// Counts YAML nodes and the maximum nesting depth, rejecting a document that exceeds either bound.
///
/// Counting rules: a scalar counts as one node; a sequence counts each of its elements; a mapping
/// counts each key and each value; the root sits at depth `1`. The walk uses an explicit stack and
/// stops at the first violation, so a hostile document cannot exhaust the process stack before the
/// bounds apply.
fn measure(value: &Value, schema: SchemaBounds) -> Result<(), DocumentError> {
    let mut pending = vec![(value, 1_usize)];
    let mut count = 0_usize;

    while let Some((node, depth)) = pending.pop() {
        if depth > schema.get(Bound::Depth) {
            return Err(DocumentError::TooDeep {
                depth,
                limit: schema.get(Bound::Depth),
            });
        }

        count += 1;
        if count > schema.get(Bound::Nodes) {
            return Err(DocumentError::TooManyNodes {
                count,
                limit: schema.get(Bound::Nodes),
            });
        }

        match node {
            Value::Sequence(items) => {
                pending.extend(items.iter().map(|item| (item, depth + 1)));
            }
            Value::Mapping(mapping) => {
                for (key, value) in mapping {
                    pending.push((key, depth + 1));
                    pending.push((value, depth + 1));
                }
            }
            Value::Tagged(tagged) => pending.push((&tagged.value, depth + 1)),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::error::ErrorCategory;

    fn limits_with(schema: SchemaBounds) -> ParseLimits {
        ParseLimits::new(16 * 1024).with_schema(schema)
    }

    /// Builds a complete document whose provider carries the given `model_allowlist` value.
    fn document_with_allowlist(value: &str) -> String {
        format!(
            "\
apiVersion: gateway/v1
request:
  max_body_bytes: 1024
  timeout_ms: 1000
plugins: []
providers:
  - name: openai-primary
    kind: openai
    base_url: https://api.openai.com
    credential_ref: env:OPENAI_API_KEY
    model_allowlist: {value}
upstreams: []
routes: []
"
        )
    }

    mod aliases {
        use super::*;

        #[test]
        fn reject_aliases_rejects_an_anchor() {
            let text = "apiVersion: gateway/v1\nlimits: &limits\n  max_body_bytes: 1\n";
            let error = reject_aliases(text).expect_err("an anchor is not allowed");
            assert!(
                matches!(error, DocumentError::AliasForbidden),
                "expected AliasForbidden, got {error:?}"
            );
            assert_eq!(error.category(), ErrorCategory::Client);
        }

        #[test]
        fn reject_aliases_rejects_an_alias_in_a_document() {
            let text = document_with_allowlist("*allowlist");
            let error = Document::parse(text.as_bytes(), limits_with(SchemaBounds::default()))
                .expect_err("an alias is not allowed");
            assert!(
                matches!(error, DocumentError::AliasForbidden),
                "expected AliasForbidden, got {error:?}"
            );
        }

        #[test]
        fn reject_aliases_ignores_indicators_inside_scalars() {
            for text in [
                "/api/*",
                "a*b",
                "&&",
                "\"*quoted\"",
                "'*single'",
                "\"a*b\"",
                "'a*b'",
                "\"&&\"",
                "\" # not a comment\"",
                "key: \"#\"",
            ] {
                assert!(
                    reject_aliases(text).is_ok(),
                    "scalar {text:?} must not be read as an alias"
                );
            }
        }

        #[test]
        fn reject_aliases_ignores_indicators_in_comments() {
            let text = "# *alias in a comment\napiVersion: gateway/v1 # *also a comment\n";
            assert!(
                reject_aliases(text).is_ok(),
                "comments carry no YAML tokens"
            );
        }

        #[test]
        fn reject_aliases_rejects_an_anchor_after_a_quoted_hash() {
            // A `#` inside a quoted scalar used to be read as a comment start, which hid the rest
            // of the line and let the anchor through to the YAML parser.
            for text in [
                "model_allowlist: [\" #\", &a gpt-4o]",
                "model_allowlist: [ \" #\" , &a gpt-4o ]",
                "model_allowlist: [\" #\", *a]",
                "key: \"#\" &a",
                "key: \"#\"&a",
                "key: [' #'] &a",
            ] {
                let error = reject_aliases(text).expect_err("the anchor must be rejected");
                assert!(
                    matches!(error, DocumentError::AliasForbidden),
                    "expected AliasForbidden for {text:?}, got {error:?}"
                );
            }
        }

        #[test]
        fn parse_rejects_an_anchor_hidden_behind_a_quoted_hash() {
            // The same bypass through the public entry point: the anchor would otherwise be
            // expanded before the node bound could see the expansion.
            let document = document_with_allowlist("[\" #\", &a gpt-4o]");
            let error = Document::parse(document.as_bytes(), limits_with(SchemaBounds::default()))
                .expect_err("an anchor is not allowed");
            assert!(
                matches!(error, DocumentError::AliasForbidden),
                "expected AliasForbidden, got {error:?}"
            );
            assert_eq!(error.category(), ErrorCategory::Client);
        }
    }

    mod bounds {
        use super::*;

        #[test]
        fn parse_rejects_a_document_deeper_than_the_depth_bound() {
            // 100 nested sequences stay under the YAML library's own recursion cap, so they reach
            // this module; the walk then stops at the first level past the bound. The counter is
            // iterative, so the depth a document can report never depends on the call stack.
            let depth = 100;
            let value = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
            let document = document_with_allowlist(&value);

            let error = Document::parse(document.as_bytes(), limits_with(SchemaBounds::default()))
                .expect_err("the nested sequence exceeds the default depth bound");
            assert!(
                matches!(
                    error,
                    DocumentError::TooDeep {
                        depth: 33,
                        limit: 32
                    }
                ),
                "expected TooDeep at depth 33, got {error:?}"
            );
            assert_eq!(error.category(), ErrorCategory::Limit);
        }

        #[test]
        fn parse_rejects_a_document_with_more_nodes_than_the_node_bound() {
            let items = (0..32)
                .map(|index| format!("model-{index}"))
                .collect::<Vec<_>>()
                .join(", ");
            let document = document_with_allowlist(&format!("[{items}]"));
            let schema = SchemaBounds::default()
                .with(Bound::Nodes, 10)
                .expect("10 is a legal bound");

            let error = Document::parse(document.as_bytes(), limits_with(schema))
                .expect_err("the allowlist exceeds the node bound");
            assert!(
                matches!(
                    error,
                    DocumentError::TooManyNodes {
                        count: 11,
                        limit: 10
                    }
                ),
                "expected TooManyNodes at count 11, got {error:?}"
            );
            assert_eq!(error.category(), ErrorCategory::Limit);
        }

        #[test]
        fn parse_accepts_a_document_within_the_default_bounds() {
            let document = document_with_allowlist("[]");
            assert!(
                Document::parse(document.as_bytes(), ParseLimits::new(16 * 1024)).is_ok(),
                "an ordinary document must fit the default bounds"
            );
        }
    }

    proptest! {
        #[test]
        fn parse_never_panics_on_arbitrary_input(
            bytes in proptest::collection::vec(any::<u8>(), 0..4096),
            max_bytes in 0usize..8192,
        ) {
            // The point of the property is that no input makes the parser panic.
            let _ = Document::parse(&bytes, ParseLimits::new(max_bytes));
        }
    }
}
