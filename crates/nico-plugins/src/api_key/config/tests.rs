//! Rule tests for the `api_key` kind's own configuration.
//!
//! These are the rules that used to live in the document model, kept here because only the kind knows
//! that a key is a reference, that a key name becomes a request identity, and that a custom header is
//! matched in lowercase. Every fixture differs from a valid entry in exactly one field.

use super::*;
use crate::{
    PluginError,
    test_support::{document, plugin},
};

/// A `config` block whose key set is `keys`, with `extra` written beside `keys`.
fn entry(extra: &str, keys: &str) -> String {
    format!("  - name: partner\n    kind: api_key\n    config:\n{extra}      keys:\n{keys}")
}

/// Reads the `partner` entry's configuration from a document holding `entry`.
fn config(entry: &str) -> ApiKeyConfig {
    let document = document(entry, &["partner"]);
    ApiKeyConfig::from_spec(plugin(&document, "partner")).expect("the fixture compiles")
}

/// Reads the `partner` entry's configuration, expecting this kind to refuse it.
fn refused(entry: &str) -> PluginError {
    let document = document(entry, &["partner"]);
    ApiKeyConfig::from_spec(plugin(&document, "partner")).expect_err("the fixture must be refused")
}

/// Asserts the error is `InvalidParameter` for `field`, and returns the reason.
fn invalid_field<'a>(error: &'a PluginError, expected: &str) -> &'a str {
    match error {
        PluginError::InvalidParameter {
            plugin,
            field,
            reason,
        } => {
            assert_eq!(plugin, "partner", "the message must name the plugin");
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
        let error = refused("  - name: partner\n    kind: api_key\n");

        invalid_field(&error, "config.keys");
        assert_eq!(error.category(), nico_core::ErrorCategory::Client);
    }

    #[test]
    fn an_empty_key_set_is_refused() {
        let error = refused(&entry("", "        []\n"));

        invalid_field(&error, "config.keys");
    }

    #[test]
    fn two_keys_with_the_same_name_are_refused() {
        let keys = concat!(
            "        - name: ci\n",
            "          secret_ref: env:KEY_ONE\n",
            "        - name: ci\n",
            "          secret_ref: env:KEY_TWO\n",
        );
        let error = refused(&entry("", keys));

        let reason = invalid_field(&error, "config.keys[].name");
        assert!(
            reason.contains("ci"),
            "the message must name the repeated name: {reason}"
        );
    }

    #[test]
    fn a_key_name_with_a_newline_is_refused() {
        // The name becomes the request identity and a log field, so a newline would let one document
        // forge a second log record.
        let keys = "        - name: \"ci\\nforged\"\n          secret_ref: env:KEY\n";
        let error = refused(&entry("", keys));

        invalid_field(&error, "config.keys[].name");
    }

    #[test]
    fn a_secret_ref_without_a_known_prefix_is_refused() {
        let keys = "        - name: ci\n          secret_ref: KEY_ONE\n";
        let error = refused(&entry("", keys));

        invalid_field(&error, "config.keys[].secret_ref");
    }

    #[test]
    fn an_empty_secret_ref_target_is_refused() {
        let keys = "        - name: ci\n          secret_ref: \"file:\"\n";
        let error = refused(&entry("", keys));

        invalid_field(&error, "config.keys[].secret_ref");
    }
}

mod tenants {
    use super::*;

    #[test]
    fn a_tenant_is_optional() {
        let keys = "        - name: ci\n          secret_ref: env:KEY_ONE\n";
        assert_eq!(config(&entry("", keys)).keys[0].tenant, None);
    }

    #[test]
    fn a_valid_tenant_is_kept() {
        let keys =
            "        - name: ci\n          tenant: acme\n          secret_ref: env:KEY_ONE\n";
        assert_eq!(
            config(&entry("", keys)).keys[0].tenant.as_deref(),
            Some("acme")
        );
    }

    #[test]
    fn invalid_tenants_are_refused() {
        for tenant in ["", "a".repeat(64).as_str(), "has space", "bad\\nvalue"] {
            let keys = format!(
                "        - name: ci\n          tenant: \"{tenant}\"\n          secret_ref: env:KEY_ONE\n"
            );
            let error = refused(&entry("", &keys));
            invalid_field(&error, "config.keys[].tenant");
        }
    }
}

mod headers {
    use super::*;

    #[test]
    fn a_custom_header_must_be_a_lowercase_header_name() {
        let keys = "        - name: ci\n          secret_ref: env:KEY_ONE\n";
        for header in ["X-Api-Key", "x api key", ""] {
            let extra = format!("      header: \"{header}\"\n");
            let error = refused(&entry(&extra, keys));
            invalid_field(&error, "config.header");
        }
    }

    #[test]
    fn a_custom_header_is_kept() {
        let keys = "        - name: ci\n          secret_ref: env:KEY_ONE\n";
        let config = config(&entry("      header: x-api-key\n", keys));

        assert_eq!(config.header.as_deref(), Some("x-api-key"));
    }

    #[test]
    fn a_header_defaults_to_none() {
        let keys = "        - name: ci\n          secret_ref: env:KEY_ONE\n";
        assert_eq!(config(&entry("", keys)).header, None);
    }
}

mod shape {
    use super::*;

    #[test]
    fn an_unknown_field_under_config_is_refused_and_named() {
        let keys = "        - name: ci\n          secret_ref: env:KEY_ONE\n";
        let error = refused(&entry("      prefix: Bearer\n", keys));

        match &error {
            PluginError::InvalidConfig { plugin, source } => {
                assert_eq!(plugin, "partner");
                assert!(
                    source.to_string().contains("prefix"),
                    "the message must name the field: {source}"
                );
            }
            other => panic!("expected InvalidConfig, got {other:?}"),
        }
    }
}
