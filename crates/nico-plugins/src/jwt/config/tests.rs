//! Rule tests for the `jwt` kind's own configuration.
//!
//! These are the rules that used to live in the document model, kept here because the kind is the only
//! thing that knows a token selects its key by `kid`, that a credential is a reference, and that a
//! `kid` is written into a log record. Every fixture differs from a valid entry in exactly one field,
//! so a failure names the rule that broke.

use super::*;
use crate::{
    PluginError,
    test_support::{document, plugin},
};

/// Reads the `jwt` entry's configuration from a document holding `entry`.
fn config(entry: &str) -> JwtConfig {
    let document = document(entry, &["jwt"]);
    JwtConfig::from_spec(plugin(&document, "jwt")).expect("the fixture compiles")
}

/// Reads the `jwt` entry's configuration, expecting this kind to refuse it.
fn refused(entry: &str) -> PluginError {
    let document = document(entry, &["jwt"]);
    JwtConfig::from_spec(plugin(&document, "jwt")).expect_err("the fixture must be refused")
}

/// Asserts the error is `InvalidParameter` for `field`, and returns the reason.
fn invalid_field<'a>(error: &'a PluginError, expected: &str) -> &'a str {
    match error {
        PluginError::InvalidParameter {
            plugin,
            field,
            reason,
        } => {
            assert_eq!(plugin, "jwt", "the message must name the plugin");
            assert_eq!(*field, expected, "wrong field in {error}");
            reason
        }
        other => panic!("expected InvalidParameter for {expected}, got {other:?}"),
    }
}

mod key_sets {
    use super::*;

    #[test]
    fn an_entry_that_writes_no_configuration_is_refused() {
        // "Wrote nothing" and "wrote an empty key set" must be the same answer: a plugin that accepts
        // nobody is a misconfiguration, not a policy.
        let error = refused("  - name: jwt\n    kind: jwt\n");

        invalid_field(&error, "config.keys");
        assert_eq!(error.category(), nico_core::ErrorCategory::Client);
    }

    #[test]
    fn an_empty_key_set_is_refused() {
        let error = refused("  - name: jwt\n    kind: jwt\n    config:\n      keys: []\n");

        invalid_field(&error, "config.keys");
    }

    #[test]
    fn a_key_set_that_writes_no_kid_is_accepted() {
        let config = config(
            "  - name: jwt\n    kind: jwt\n    config:\n      keys:\n        - secret_ref: env:JWT_SECRET\n",
        );

        assert_eq!(config.keys.len(), 1);
        assert_eq!(config.keys[0].kid, None);
        assert_eq!(config.keys[0].secret_ref, "env:JWT_SECRET");
    }
}

mod credentials {
    use super::*;

    #[test]
    fn a_plaintext_secret_is_not_a_field_of_the_document() {
        // `config` is deserialized with `deny_unknown_fields`, so the field that would hold the
        // secret itself does not exist and the attempt is refused instead of ignored.
        let error = refused(
            "  - name: jwt\n    kind: jwt\n    config:\n      keys:\n        - secret: super-secret-value\n",
        );

        match &error {
            PluginError::InvalidConfig { plugin, source } => {
                assert_eq!(plugin, "jwt");
                assert!(
                    source.to_string().contains("secret"),
                    "the field must be named: {source}"
                );
            }
            other => panic!("expected InvalidConfig, got {other:?}"),
        }
    }

    #[test]
    fn a_secret_ref_without_a_known_prefix_is_refused() {
        let error = refused(
            "  - name: jwt\n    kind: jwt\n    config:\n      keys:\n        - kid: k1\n          secret_ref: JWT_SECRET\n",
        );

        invalid_field(&error, "config.keys[].secret_ref");
    }

    #[test]
    fn an_empty_secret_ref_target_is_refused() {
        let error = refused(
            "  - name: jwt\n    kind: jwt\n    config:\n      keys:\n        - kid: k1\n          secret_ref: \"env:\"\n",
        );

        invalid_field(&error, "config.keys[].secret_ref");
    }

    #[test]
    fn a_file_reference_is_accepted() {
        let config = config(
            "  - name: jwt\n    kind: jwt\n    config:\n      keys:\n        - kid: k1\n          secret_ref: file:/run/secrets/jwt\n",
        );

        assert_eq!(config.keys[0].secret_ref, "file:/run/secrets/jwt");
    }
}

mod kids {
    use super::*;

    #[test]
    fn a_duplicate_kid_is_refused() {
        let error = refused(concat!(
            "  - name: jwt\n",
            "    kind: jwt\n",
            "    config:\n",
            "      keys:\n",
            "        - kid: k1\n",
            "          secret_ref: env:FIRST\n",
            "        - kid: k1\n",
            "          secret_ref: env:SECOND\n",
        ));

        let reason = invalid_field(&error, "config.keys[].kid");
        assert!(
            reason.contains("k1"),
            "the message must name the repeated kid: {reason}"
        );
    }

    #[test]
    fn several_keys_may_mix_a_named_and_an_unnamed_one() {
        // Whether an unnamed key may coexist is this kind's rule, and it says yes: the compiler is the
        // layer that refuses a *multi-key* set with an unnamed key, because a token could never select
        // it.
        let config = config(concat!(
            "  - name: jwt\n",
            "    kind: jwt\n",
            "    config:\n",
            "      keys:\n",
            "        - secret_ref: env:FIRST\n",
            "        - kid: k2\n",
            "          secret_ref: env:SECOND\n",
        ));

        assert_eq!(config.keys.len(), 2);
        assert_eq!(config.keys[1].kid.as_deref(), Some("k2"));
    }

    #[test]
    fn a_kid_with_a_newline_is_refused() {
        // A `kid` is written into a log record, so a newline would let one document forge a second
        // record.
        let error = refused(concat!(
            "  - name: jwt\n",
            "    kind: jwt\n",
            "    config:\n",
            "      keys:\n",
            "        - kid: \"k1\\nforged\"\n",
            "          secret_ref: env:JWT_SECRET\n",
        ));

        invalid_field(&error, "config.keys[].kid");
    }

    #[test]
    fn a_kid_boundary_is_63_bytes() {
        let at_bound = "k".repeat(63);
        let config = config(&format!(
            "  - name: jwt\n    kind: jwt\n    config:\n      keys:\n        - kid: {at_bound}\n          secret_ref: env:JWT_SECRET\n"
        ));
        assert_eq!(config.keys[0].kid.as_deref(), Some(at_bound.as_str()));

        let over_bound = "k".repeat(64);
        let error = refused(&format!(
            "  - name: jwt\n    kind: jwt\n    config:\n      keys:\n        - kid: {over_bound}\n          secret_ref: env:JWT_SECRET\n"
        ));
        invalid_field(&error, "config.keys[].kid");
    }
}

mod shape {
    use super::*;

    #[test]
    fn an_unknown_field_under_config_is_refused_and_named() {
        let error = refused(concat!(
            "  - name: jwt\n",
            "    kind: jwt\n",
            "    config:\n",
            "      keys:\n",
            "        - kid: k1\n",
            "          secret_ref: env:JWT_SECRET\n",
            "      backoff: 3\n",
        ));

        match &error {
            PluginError::InvalidConfig { plugin, source } => {
                assert_eq!(plugin, "jwt");
                assert!(
                    source.to_string().contains("backoff"),
                    "the message must name the field: {source}"
                );
            }
            other => panic!("expected InvalidConfig, got {other:?}"),
        }
        assert_eq!(error.category(), nico_core::ErrorCategory::Client);
    }

    #[test]
    fn a_config_that_is_not_a_mapping_is_refused() {
        let error = refused("  - name: jwt\n    kind: jwt\n    config: just-a-string\n");

        assert!(
            matches!(error, PluginError::InvalidConfig { .. }),
            "expected InvalidConfig, got {error:?}"
        );
    }
}
