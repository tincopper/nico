//! Rule tests for the `rate_limit` kind's own configuration: the numbers a bucket cannot work with,
//! and the defaults the document may leave out.
//!
//! Each fixture differs from a valid entry in exactly one field, so a failure names the rule that
//! broke.

use super::*;
use crate::{
    PluginError,
    test_support::{document, plugin},
};

/// A `rate_limit` entry with the given numbers and optional extra fields.
fn entry(burst: &str, per_second: &str, extra: &str) -> String {
    format!(
        "  - name: quota\n    kind: rate_limit\n    config:\n      burst: {burst}\n      per_second: {per_second}\n{extra}"
    )
}

/// Reads the `quota` entry's configuration from a document holding `entry`.
fn config(entry: &str) -> RateLimitConfig {
    let document = document(entry, &["quota"]);
    RateLimitConfig::from_spec(plugin(&document, "quota")).expect("the fixture compiles")
}

/// Reads the `quota` entry's configuration, expecting this kind to refuse it.
fn refused(entry: &str) -> PluginError {
    let document = document(entry, &["quota"]);
    RateLimitConfig::from_spec(plugin(&document, "quota")).expect_err("the fixture must be refused")
}

/// Asserts the error is `InvalidParameter` for `field`.
fn invalid_field(error: &PluginError, expected: &str) {
    match error {
        PluginError::InvalidParameter { plugin, field, .. } => {
            assert_eq!(plugin, "quota", "the message must name the plugin");
            assert_eq!(*field, expected, "wrong field in {error}");
        }
        other => panic!("expected InvalidParameter for {expected}, got {other:?}"),
    }
}

mod buckets {
    use super::*;

    #[test]
    fn a_zero_burst_is_refused() {
        let error = refused(&entry("0", "20", ""));

        invalid_field(&error, "config.burst");
        assert_eq!(
            error.category(),
            nico_core::ErrorCategory::Client,
            "a value the operator wrote is a document fault, whatever it bounds"
        );
    }

    #[test]
    fn a_zero_rate_is_refused() {
        let error = refused(&entry("100", "0", ""));

        invalid_field(&error, "config.per_second");
    }

    #[test]
    fn a_max_keys_outside_the_accepted_range_is_refused() {
        for max_keys in ["0", "100001"] {
            let error = refused(&entry(
                "100",
                "20",
                &format!("      max_keys: {max_keys}\n"),
            ));
            invalid_field(&error, "config.max_keys");
        }
    }

    #[test]
    fn the_hard_ceiling_and_the_smallest_table_are_accepted() {
        for max_keys in ["1", "100000"] {
            let config = config(&entry(
                "100",
                "20",
                &format!("      max_keys: {max_keys}\n"),
            ));
            assert_eq!(
                config.max_keys(),
                max_keys.parse::<u32>().expect("the fixture is a number")
            );
        }
    }
}

mod defaults {
    use super::*;

    #[test]
    fn an_omitted_table_bound_and_key_source_take_the_defaults() {
        let config = config(&entry("100", "20", ""));

        assert_eq!(config.max_keys, None, "the entry wrote no bound");
        assert_eq!(config.max_keys(), DEFAULT_MAX_KEYS);
        assert_eq!(config.identity, None, "the entry wrote no key source");
        assert_eq!(config.identity(), IdentitySource::Subject);
        assert_eq!(DEFAULT_MAX_KEYS, 10_000);
        assert_eq!(MAX_KEYS_CEILING, 100_000);
    }

    #[test]
    fn a_pinned_key_source_replaces_the_default() {
        assert_eq!(
            config(&entry("100", "20", "      identity: ip\n")).identity(),
            IdentitySource::Ip
        );
    }
}

mod shape {
    use super::*;

    #[test]
    fn an_unknown_field_under_config_is_refused_and_named() {
        let error = refused(&entry("100", "20", "      window_ms: 1000\n"));

        match &error {
            PluginError::InvalidConfig { plugin, source } => {
                assert_eq!(plugin, "quota");
                assert!(
                    source.to_string().contains("window_ms"),
                    "the message must name the field: {source}"
                );
            }
            other => panic!("expected InvalidConfig, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_key_source_is_refused_while_the_configuration_is_read() {
        let error = refused(&entry("100", "20", "      identity: tenant\n"));

        assert!(
            matches!(error, PluginError::InvalidConfig { .. }),
            "an unknown spelling is a schema fault, refused before any range rule: {error:?}"
        );
    }

    #[test]
    fn an_entry_that_writes_no_configuration_is_refused() {
        let document = document("  - name: quota\n    kind: rate_limit\n", &["quota"]);
        let error = RateLimitConfig::from_spec(plugin(&document, "quota"))
            .expect_err("a bucket with no numbers cannot be used");

        assert!(
            matches!(error, PluginError::InvalidConfig { .. }),
            "a missing field is a configuration-shape fault: {error:?}"
        );
    }
}
