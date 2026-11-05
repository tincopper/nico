//! What a credential reference is: `env:NAME` or `file:/path`, naming a non-empty target.
//!
//! A document never holds a secret, only the name of one. That rule is one rule, shared by every
//! field that references a credential — a provider's `credential_ref`, a plugin's `secret_ref` — and
//! it is shared **across crates**: the document model owns the provider side, and the crate that
//! implements a plugin kind owns its own fields, so both need the same predicate rather than two
//! copies that would eventually disagree about, say, whether `env:` with an empty target counts.
//!
//! The predicate answers only "is this a reference this build can resolve". It never resolves it:
//! reading the value is the job of the crate that makes the call, at publication time, and a failure
//! there rejects the whole document rather than one field. The error text is the caller's, because
//! only the caller knows which field and which configuration key it is reporting.

/// Reports whether `reference` names a credential source this build can read.
///
/// A reference is `env:VAR` or `file:/path` with a non-empty target and no control characters: a
/// control character would let a document forge the log line that names the reference when it cannot
/// be read, and an empty target names nothing at all.
///
/// # Examples
///
/// ```
/// use nico_core::is_secret_ref;
///
/// assert!(is_secret_ref("env:OPENAI_API_KEY"));
/// assert!(is_secret_ref("file:/run/secrets/partner"));
/// assert!(!is_secret_ref("OPENAI_API_KEY"));
/// assert!(!is_secret_ref("env:"));
/// ```
#[must_use]
pub fn is_secret_ref(reference: &str) -> bool {
    const SCHEMES: [&str; 2] = ["env:", "file:"];
    let Some(target) = SCHEMES
        .iter()
        .find_map(|scheme| reference.strip_prefix(scheme))
    else {
        return false;
    };
    !target.is_empty() && !target.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_sources_are_accepted() {
        for reference in [
            "env:JWT_SECRET",
            "file:/run/secrets/jwt",
            "file:relative/key",
        ] {
            assert!(is_secret_ref(reference), "{reference} is a reference");
        }
    }

    #[test]
    fn a_literal_value_is_not_a_reference() {
        // The mistake this refuses is the one that matters: writing the secret instead of its name.
        for reference in ["JWT_SECRET", "super-secret-value", "", "file", "env"] {
            assert!(!is_secret_ref(reference), "{reference} names no source");
        }
    }

    #[test]
    fn an_empty_target_is_not_a_reference() {
        for reference in ["env:", "file:"] {
            assert!(!is_secret_ref(reference), "{reference} names nothing");
        }
    }

    #[test]
    fn a_control_character_is_refused() {
        // The reference is written into a publication log when it cannot be read, so a newline would
        // let one configuration forge a second log record.
        for reference in ["env:A\nforged", "file:/run/secrets/a\u{0}b"] {
            assert!(
                !is_secret_ref(reference),
                "{reference:?} carries a control character"
            );
        }
    }
}
