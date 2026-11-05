//! Tests for the compiled parameters: the values a filter reads that came out of the document.
//!
//! A range the kind's own configuration already refuses cannot be produced here — a `JwtConfig` only
//! exists after the entry's `config` was read — so these tests assert the compiled shape (durations,
//! non-zero numbers, a closed key source, the reference order) and the rules this layer enforces on
//! top of the configuration: a multi-key set needs a `kid` on every key, and a bucket parameter has to
//! be convertible into the non-zero type the limiter holds.

use std::time::Duration;

use nico_core::ErrorCategory;

use super::{KeyBy, LimitParameters};
use crate::{
    PluginError,
    api_key::ApiKeyConfig,
    jwt::JwtConfig,
    rate_limit::RateLimitConfig,
    spec::{ApiKeySpec, JwtSpec, RateLimitSpec},
    test_support::{document, plugin},
};

/// Compiles the only `jwt` entry of a document holding `entry`.
fn jwt(entry: &str) -> JwtSpec {
    let document = document(entry, &["jwt"]);
    let plugin = plugin(&document, "jwt");
    let config = JwtConfig::from_spec(plugin).expect("the fixture configuration compiles");
    JwtSpec::compile(plugin, &config).expect("the fixture compiles")
}

/// Compiles the only `jwt` entry of `entry`, expecting this layer to refuse it.
fn jwt_error(entry: &str) -> PluginError {
    let document = document(entry, &["jwt"]);
    let plugin = plugin(&document, "jwt");
    let config = JwtConfig::from_spec(plugin).expect("the fixture configuration compiles");
    JwtSpec::compile(plugin, &config)
        .err()
        .expect("the fixture must not compile")
}

/// Compiles the only `rate_limit` entry of a document holding `entry`.
fn limit(entry: &str) -> RateLimitSpec {
    let document = document(entry, &["quota"]);
    let plugin = plugin(&document, "quota");
    let config = RateLimitConfig::from_spec(plugin).expect("the fixture configuration compiles");
    RateLimitSpec::compile(plugin, &config).expect("the fixture compiles")
}

/// A `jwt` entry with one key and no rejection override.
const JWT_ONE_KEY: &str = concat!(
    "  - name: jwt\n",
    "    kind: jwt\n",
    "    config:\n",
    "      keys:\n",
    "        - kid: k1\n",
    "          secret_ref: env:JWT_SECRET\n",
);

/// A `rate_limit` entry with the documented defaults written out.
const LIMIT_ENTRY: &str = concat!(
    "  - name: quota\n",
    "    kind: rate_limit\n",
    "    config:\n",
    "      burst: 3\n",
    "      per_second: 2\n",
);

mod claims {
    use super::*;

    #[test]
    fn the_leeway_defaults_to_thirty_seconds() {
        assert_eq!(jwt(JWT_ONE_KEY).leeway, Duration::from_secs(30));
    }

    #[test]
    fn a_pinned_leeway_replaces_the_default() {
        let entry = format!("{JWT_ONE_KEY}      leeway_seconds: 5\n");

        assert_eq!(jwt(&entry).leeway, Duration::from_secs(5));
    }

    #[test]
    fn an_issuer_and_an_audience_are_carried_only_when_the_entry_pins_them() {
        let bare = jwt(JWT_ONE_KEY);
        assert!(bare.issuer.is_none() && bare.audience.is_none());

        let pinned = jwt(&format!(
            "{JWT_ONE_KEY}      issuer: https://id.example\n      audience: api\n"
        ));
        assert_eq!(pinned.issuer.as_deref(), Some("https://id.example"));
        assert_eq!(pinned.audience.as_deref(), Some("api"));
    }

    #[test]
    fn the_instance_name_is_the_document_name() {
        // It becomes the identity's `source` and the `authn` log field, so it must be the name an
        // operator wrote, not the kind.
        assert_eq!(jwt(JWT_ONE_KEY).instance.as_ref(), "jwt");
    }
}

mod key_selection {
    use super::*;

    #[test]
    fn a_multi_key_set_requires_a_kid_on_every_key() {
        // The configuration type accepts this shape (a `kid` is optional there); this layer is the one
        // that knows a token selects its key by `kid`, so a key without one could never be selected
        // and a rotation would silently leave the old credential unable to verify anything.
        let entry = concat!(
            "  - name: jwt\n",
            "    kind: jwt\n",
            "    config:\n",
            "      keys:\n",
            "        - secret_ref: env:OLD_SECRET\n",
            "        - kid: k2\n",
            "          secret_ref: env:NEW_SECRET\n",
        );

        let error = jwt_error(entry);

        match &error {
            PluginError::InvalidParameter {
                plugin,
                field,
                reason,
            } => {
                assert_eq!(plugin, "jwt");
                assert_eq!(*field, "config.keys[].kid");
                assert!(
                    reason.contains("env:OLD_SECRET"),
                    "the message must name the key that cannot be used: {reason}"
                );
            }
            other => panic!("expected InvalidParameter, got {other:?}"),
        }
        assert_eq!(
            error.category(),
            ErrorCategory::Client,
            "the document is the operator's, not a dependency's"
        );
    }

    #[test]
    fn a_single_key_may_omit_the_kid() {
        let spec = jwt(concat!(
            "  - name: jwt\n",
            "    kind: jwt\n",
            "    config:\n",
            "      keys:\n",
            "        - secret_ref: env:JWT_SECRET\n",
        ));

        assert!(
            spec.keys[0].kid.is_none(),
            "a single key answers every token, so it needs no name"
        );
    }
}

mod credentials {
    use super::*;

    #[test]
    fn jwt_key_references_are_iterated_in_document_order() {
        let spec = jwt(concat!(
            "  - name: jwt\n",
            "    kind: jwt\n",
            "    config:\n",
            "      keys:\n",
            "        - kid: k1\n",
            "          secret_ref: env:FIRST\n",
            "        - kid: k2\n",
            "          secret_ref: file:/run/secrets/second\n",
        ));

        assert_eq!(
            spec.credentials().collect::<Vec<_>>(),
            vec!["env:FIRST", "file:/run/secrets/second"],
            "the materializer must read the references in the order the filter expects the values"
        );
    }

    #[test]
    fn api_key_references_are_iterated_in_entry_order() {
        let document = document(
            concat!(
                "  - name: partner\n",
                "    kind: api_key\n",
                "    config:\n",
                "      keys:\n",
                "        - name: acme\n",
                "          secret_ref: env:ACME\n",
                "        - name: globex\n",
                "          secret_ref: env:GLOBEX\n",
            ),
            &["partner"],
        );
        let plugin = plugin(&document, "partner");
        let config = ApiKeyConfig::from_spec(plugin).expect("the fixture configuration compiles");
        let spec = ApiKeySpec::compile(plugin, &config).expect("the fixture compiles");

        assert_eq!(
            spec.credentials().collect::<Vec<_>>(),
            vec!["env:ACME", "env:GLOBEX"]
        );
        assert_eq!(
            spec.entries
                .iter()
                .map(|entry| entry.name.as_ref())
                .collect::<Vec<_>>(),
            vec!["acme", "globex"],
            "each key's name is the subject it authenticates as"
        );
    }

    #[test]
    fn a_custom_header_is_carried_as_written() {
        let document = document(
            concat!(
                "  - name: partner\n",
                "    kind: api_key\n",
                "    config:\n",
                "      header: x-api-key\n",
                "      keys:\n",
                "        - name: acme\n",
                "          secret_ref: env:ACME\n",
            ),
            &["partner"],
        );
        let plugin = plugin(&document, "partner");
        let config = ApiKeyConfig::from_spec(plugin).expect("the fixture configuration compiles");
        let spec = ApiKeySpec::compile(plugin, &config).expect("the fixture compiles");

        assert_eq!(spec.header.as_deref(), Some("x-api-key"));
    }
}

mod buckets {
    use super::*;

    #[test]
    fn a_zero_bucket_size_is_refused_and_the_field_is_named() {
        for (field, parameters) in [
            ("burst", LimitParameters::new("quota", 0, 1, 100)),
            ("per_second", LimitParameters::new("quota", 1, 0, 100)),
            ("max_keys", LimitParameters::new("quota", 1, 1, 0)),
        ] {
            match parameters.expect_err("a zero cannot be used") {
                PluginError::InvalidParameter {
                    plugin: name,
                    field: broken,
                    ..
                } => {
                    assert_eq!(name, "quota");
                    assert_eq!(broken, field);
                }
                other => panic!("expected InvalidParameter for {field}, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_zero_value_is_reported_as_the_callers_mistake() {
        let error = LimitParameters::new("quota", 0, 1, 100).expect_err("zero burst");
        assert_eq!(error.category(), ErrorCategory::Client);
    }

    #[test]
    fn the_documented_defaults_are_carried_from_the_entry() {
        let spec = limit(LIMIT_ENTRY);

        assert_eq!(spec.parameters().burst().get(), 3);
        assert_eq!(spec.parameters().per_second().get(), 2);
        assert_eq!(
            spec.parameters().max_keys().get(),
            10_000,
            "the kind's configuration fills the default bound before this layer sees it"
        );
    }

    #[test]
    fn parameters_are_equal_only_when_every_bucket_number_is() {
        let base = LimitParameters::new("quota", 3, 2, 100).expect("the fixture is valid");

        assert_eq!(
            base,
            LimitParameters::new("quota", 3, 2, 100).expect("the fixture is valid")
        );
        for other in [
            LimitParameters::new("quota", 4, 2, 100).expect("valid"),
            LimitParameters::new("quota", 3, 3, 100).expect("valid"),
            LimitParameters::new("quota", 3, 2, 101).expect("valid"),
        ] {
            assert_ne!(
                base, other,
                "a changed quota must start new buckets rather than reuse the old ones"
            );
        }
    }
}

mod keying {
    use super::*;

    #[test]
    fn the_identity_source_defaults_to_the_subject() {
        assert_eq!(limit(LIMIT_ENTRY).key_by, KeyBy::Subject);
    }

    #[test]
    fn a_pinned_identity_source_replaces_the_default() {
        let spec = limit(&format!("{LIMIT_ENTRY}      identity: ip\n"));

        assert_eq!(spec.key_by, KeyBy::Ip);
    }
}
