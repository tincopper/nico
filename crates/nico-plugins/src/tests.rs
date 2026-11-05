//! Tests for the crate's publication-failure vocabulary.
//!
//! The subject is what an operator reads when a document is refused: the message must name the plugin
//! and the field or reference at fault, classify the cause correctly, and never grow a second channel
//! that quotes a credential.

use std::io;

use nico_core::ErrorCategory;

use crate::PluginError;

/// Returns an error naming `plugin` and `reference` whose source text is a plausible secret.
fn secret_ref_failure() -> PluginError {
    PluginError::SecretRef {
        plugin: "public-jwt".to_owned(),
        reference: "env:JWT_SECRET".to_owned(),
        cause: "the environment variable is not set".into(),
        source: Box::new(io::Error::new(
            io::ErrorKind::NotFound,
            "the value was a-shared-secret",
        )),
    }
}

#[test]
fn a_secret_reference_failure_names_the_plugin_and_the_reference() {
    let error = secret_ref_failure();
    let rendered = error.to_string();

    assert!(
        rendered.contains("public-jwt"),
        "an operator must know which plugin: {rendered}"
    );
    assert!(
        rendered.contains("env:JWT_SECRET"),
        "and which reference: {rendered}"
    );
}

#[test]
fn a_secret_reference_failure_does_not_render_its_source() {
    // The source is kept for a caller that wants the cause; the message a log records must not be a
    // second place a value could appear, so it never interpolates it.
    let error = secret_ref_failure();
    let rendered = error.to_string();

    assert!(
        !rendered.contains("a-shared-secret"),
        "the underlying error's text must not reach the message: {rendered}"
    );
}

#[test]
fn a_credential_reference_read_failure_is_the_dependency_failing() {
    // A secret store that cannot be read is not the operator's typo: the document may be perfectly
    // right, and the environment is what changed.
    assert_eq!(
        secret_ref_failure().category(),
        ErrorCategory::Upstream,
        "a credential that cannot be read is an unavailable dependency"
    );
}

#[test]
fn every_document_fault_is_the_callers_mistake() {
    let faults = [
        PluginError::UnknownVariable {
            plugin: "jwt".to_owned(),
            variable: "secret".to_owned(),
        },
        PluginError::UnterminatedTemplate {
            plugin: "jwt".to_owned(),
        },
        PluginError::ContentTypeWithoutBody {
            plugin: "jwt".to_owned(),
        },
        PluginError::InvalidParameter {
            plugin: "quota".to_owned(),
            field: "burst",
            reason: "must be greater than zero".into(),
        },
    ];

    for error in faults {
        assert_eq!(
            error.category(),
            ErrorCategory::Client,
            "`{error}` is a document fault"
        );
    }
}

#[test]
fn a_credential_count_mismatch_is_reported_as_our_own_defect() {
    let error = PluginError::CredentialCountMismatch {
        plugin: "public-jwt".to_owned(),
        declared: 2,
        materialized: 1,
    };

    assert_eq!(error.category(), ErrorCategory::Internal);
    let rendered = error.to_string();
    assert!(
        rendered.contains('2') && rendered.contains('1'),
        "both counts must be named so the compiler bug is diagnosable: {rendered}"
    );
}

#[test]
fn an_unknown_variable_message_quotes_the_whitelist() {
    let error = PluginError::UnknownVariable {
        plugin: "jwt".to_owned(),
        variable: "secret".to_owned(),
    };
    let rendered = error.to_string();

    for variable in ["code", "message", "trace_id"] {
        assert!(
            rendered.contains(variable),
            "the message must say what is allowed: {rendered}"
        );
    }
    assert!(
        rendered.contains("${secret}"),
        "and what was written: {rendered}"
    );
}

/// What the process gets when it asks which kinds this build implements.
mod kinds {
    use nico_core::ErrorCategory;
    use nico_http::{PluginBuildError, PluginFactory, PluginPhase};

    use crate::{
        api_key::ApiKeyFactory,
        builtin_registry,
        jwt::JwtFactory,
        rate_limit::RateLimitFactory,
        test_support::{document, plugin},
    };

    /// Builds `spec` through `factory`, expecting the kind to refuse it.
    fn refuse(factory: &dyn PluginFactory, spec: &nico_core::PluginSpec) -> PluginBuildError {
        match factory.build(spec) {
            Ok(_plugin) => panic!("the fixture configuration must be refused"),
            Err(error) => error,
        }
    }

    #[test]
    fn every_built_in_kind_is_registered_with_its_ordering_class() {
        let registry = builtin_registry();

        assert_eq!(registry.len(), 3, "the three built-in kinds");
        for (kind, phase) in [
            ("jwt", PluginPhase::Authenticate),
            ("api_key", PluginPhase::Authenticate),
            ("rate_limit", PluginPhase::Limit),
        ] {
            let factory = registry
                .factory(kind)
                .unwrap_or_else(|| panic!("`{kind}` must be registered"));
            assert_eq!(factory.kind(), kind);
            assert_eq!(factory.phase(), phase, "`{kind}` declares its class");
        }
        assert!(
            registry.factory("wasm").is_none(),
            "a kind this build does not implement is not in the registry"
        );
    }

    #[test]
    fn a_kinds_default_config_is_json_its_own_type_accepts() {
        // The text is what documentation and `PluginInfo` show, so a reader must be able to paste it
        // into a document. The kind's own configuration type is the judge, not this test.
        let jwt: crate::jwt::JwtConfig =
            serde_norway::from_str(crate::jwt::DEFAULT_CONFIG).expect("jwt's example is JSON");
        jwt.validate("example").expect("jwt's example is a key set");

        let api_key: crate::api_key::ApiKeyConfig =
            serde_norway::from_str(crate::api_key::DEFAULT_CONFIG)
                .expect("api_key's example is JSON");
        api_key
            .validate("example")
            .expect("api_key's example is a key set");

        let quota: crate::rate_limit::RateLimitConfig =
            serde_norway::from_str(crate::rate_limit::DEFAULT_CONFIG)
                .expect("rate_limit's example is JSON");
        quota
            .validate("example")
            .expect("rate_limit's example is a quota");

        for (factory, info_default) in [
            (
                JwtFactory.default_config(),
                crate::jwt::INFO.default_config(),
            ),
            (
                ApiKeyFactory.default_config(),
                crate::api_key::INFO.default_config(),
            ),
            (
                RateLimitFactory::new().default_config(),
                crate::rate_limit::INFO.default_config(),
            ),
        ] {
            assert!(
                serde_json::from_str::<serde_json::Value>(factory).is_ok(),
                "the factory's text must be JSON, not YAML: {factory}"
            );
            assert_eq!(
                factory, info_default,
                "one text, reported by the factory and by the plugin's info"
            );
        }
    }

    #[test]
    fn a_built_plugin_reports_its_instance_name_not_its_kind() {
        let document = document(
            concat!(
                "  - name: quota-free\n",
                "    kind: rate_limit\n",
                "    config:\n",
                "      burst: 5\n",
                "      per_second: 2\n",
            ),
            &[],
        );

        let built = RateLimitFactory::new()
            .build(plugin(&document, "quota-free"))
            .expect("the fixture builds");

        assert_eq!(
            built.name(),
            "quota-free",
            "the instance name is what a log line and a bucket key are attributed to"
        );
        assert_eq!(
            built.info().default_config(),
            RateLimitFactory::new().default_config()
        );
    }

    #[test]
    fn the_built_in_kinds_declare_only_the_request_stage() {
        // None of the three rewrites a response byte, so none may claim `RESPONSE_BODY`: that claim
        // removes the upstream `content-length` from every route that names the kind and is the only
        // phase declaration with a consequence outside the plugin.
        for (kind, info) in [
            ("jwt", crate::jwt::INFO),
            ("api_key", crate::api_key::INFO),
            ("rate_limit", crate::rate_limit::INFO),
        ] {
            assert!(
                info.phases().contains(nico_http::PluginPhases::REQUEST),
                "{kind} runs in on_request"
            );
            assert!(
                !info
                    .phases()
                    .contains(nico_http::PluginPhases::RESPONSE_BODY),
                "{kind} must not force every response it touches to be chunked"
            );
        }
    }

    #[test]
    fn a_factory_refuses_a_configuration_field_the_kind_does_not_have() {
        let document = document(
            concat!(
                "  - name: quota\n",
                "    kind: rate_limit\n",
                "    config:\n",
                "      burst: 5\n",
                "      per_second: 2\n",
                "      birst: 1\n",
            ),
            &[],
        );

        let error = refuse(&RateLimitFactory::new(), plugin(&document, "quota"));

        assert_eq!(error.plugin(), "quota");
        assert!(
            error.reason().contains("birst"),
            "the reason must name the field path: {}",
            error.reason()
        );
        assert_eq!(error.category(), ErrorCategory::Client);
    }

    #[test]
    fn a_factory_keeps_the_category_of_a_credential_it_cannot_read() {
        // The loading seam carries text, but losing the category would report an unavailable secret
        // store as a document typo.
        let document = document(
            concat!(
                "  - name: api-key\n",
                "    kind: api_key\n",
                "    config:\n",
                "      keys:\n",
                "        - name: partner\n",
                "          secret_ref: file:/nonexistent/nico-plugins-fixture/key\n",
            ),
            &[],
        );

        let error = refuse(&ApiKeyFactory, plugin(&document, "api-key"));

        assert_eq!(error.category(), ErrorCategory::Upstream);
        assert!(
            error
                .reason()
                .contains("file:/nonexistent/nico-plugins-fixture/key"),
            "the reason must name the reference: {}",
            error.reason()
        );
    }
}
