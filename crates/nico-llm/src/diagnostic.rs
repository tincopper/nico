//! What a diagnostic may quote.
//!
//! Every refusal in this crate names the field it refused and never its value, because a value is the
//! caller's prompt or the provider's answer. A *name* is untrusted too: a JSON member name can carry a
//! newline, kilobytes of text or bytes that are not printable, and a sentence built from it lands in a
//! log record. The rule is therefore one rule, applied wherever a refusal quotes a name — the client
//! codec's unknown wire field, the provider adapter's unknown document field, and the provider's own
//! error code.
//!
//! A name that fails the rule is dropped from the sentence rather than excerpted: half a field name
//! names a field that does not exist.

/// Longest member name a diagnostic quotes.
pub(crate) const MAX_QUOTED_FIELD_BYTES: usize = 64;

/// Returns `text` when a diagnostic may quote it.
///
/// `limit` is the bound the caller's own field defines — a document member name has one and a
/// provider error code another, each with its own source of truth. What is shared is the shape:
/// non-empty printable ASCII, so the text cannot forge a second log record or bloat the one it is in.
pub(crate) fn quoted(text: &str, limit: usize) -> Option<&str> {
    (!text.is_empty() && text.len() <= limit && text.bytes().all(|byte| byte.is_ascii_graphic()))
        .then_some(text)
}

#[cfg(test)]
mod tests {
    use super::{MAX_QUOTED_FIELD_BYTES, quoted};

    #[test]
    fn a_plain_name_is_quoted() {
        for name in ["logprobs", "tools[].function.strict", "a", "CODE_1"] {
            assert_eq!(
                quoted(name, MAX_QUOTED_FIELD_BYTES),
                Some(name),
                "{name} is a name"
            );
        }
    }

    #[test]
    fn a_name_that_could_forge_a_record_is_not_quoted() {
        for name in ["a\nforged", "a\tb", "a\u{0}b"] {
            assert_eq!(
                quoted(name, MAX_QUOTED_FIELD_BYTES),
                None,
                "{name:?} carries a control character"
            );
        }
    }

    #[test]
    fn text_outside_ascii_is_not_quoted() {
        assert_eq!(quoted("字段", MAX_QUOTED_FIELD_BYTES), None);
    }

    #[test]
    fn an_empty_name_is_not_quoted() {
        assert_eq!(quoted("", MAX_QUOTED_FIELD_BYTES), None);
    }

    #[test]
    fn the_bound_is_the_one_the_caller_named() {
        let at_bound = "a".repeat(MAX_QUOTED_FIELD_BYTES);
        assert!(quoted(&at_bound, MAX_QUOTED_FIELD_BYTES).is_some());
        let over_bound = "a".repeat(MAX_QUOTED_FIELD_BYTES + 1);
        assert_eq!(quoted(&over_bound, MAX_QUOTED_FIELD_BYTES), None);
    }
}
