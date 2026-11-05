//! Rule tests for the plugin **envelope**: the name a route references and the rejection
//! presentation.
//!
//! Every fixture differs from a valid entry in exactly one field, so a failure names the rule that
//! broke. Whether a route names a plugin that exists is `reference`'s tests; whether an envelope
//! field is unknown, or a `kind` is empty, is the schema's, covered beside [`Document`].
//!
//! Nothing here asserts a kind-level rule. `config` is opaque to this crate, so "a JWT needs a key
//! set" or "a bucket size is not zero" is checked where the kind is implemented — same publication
//! stage, but a single copy of each rule instead of two that could drift apart.
//!
//! [`Document`]: crate::config::Document

use super::*;

/// A valid `jwt` entry with an envelope-level `error_response` spliced under it.
fn entry(response: &str) -> String {
    format!(
        concat!(
            "  - name: public-jwt\n",
            "    kind: jwt\n",
            "    config:\n",
            "      keys:\n",
            "        - secret_ref: env:JWT_SECRET\n",
            "    error_response:\n",
            "{}",
        ),
        response
    )
}

mod names {
    use super::*;

    #[test]
    fn a_plugin_name_is_kebab_case_like_every_other_name() {
        let plugins = "  - name: Public_JWT\n    kind: jwt\n";
        let error = with_plugins(plugins).expect_err("a plugin name is an identifier");

        assert!(
            matches!(
                error,
                DocumentError::InvalidField {
                    field: "plugins[].name",
                    ..
                }
            ),
            "expected InvalidField for the plugin name, got {error:?}"
        );
        assert_eq!(error.category(), ErrorCategory::Client);
    }
}

mod rejection_presentation {
    use super::*;
    use crate::config::ErrorResponse;

    #[test]
    fn a_status_outside_4xx_is_rejected() {
        for status in ["399", "200", "500", "301"] {
            let response = format!("      status: {status}\n");
            let error = with_plugins(&entry(&response))
                .expect_err("a rejection is the caller's fault, so it is a 4xx");
            assert!(
                matches!(
                    error,
                    DocumentError::InvalidField {
                        field: "plugins[].error_response.status",
                        ..
                    }
                ),
                "expected InvalidField for status {status}, got {error:?}"
            );
            assert_eq!(error.category(), ErrorCategory::Client);
        }
    }

    #[test]
    fn every_4xx_status_is_accepted() {
        for status in ["400", "401", "403", "429", "499"] {
            let response = format!("      status: {status}\n");
            let document = with_plugins(&entry(&response)).expect("a 4xx status is a rejection");
            assert_eq!(
                document.plugins()[0]
                    .error_response()
                    .and_then(ErrorResponse::status),
                Some(status.parse::<u16>().expect("the fixture is a number"))
            );
        }
    }

    #[test]
    fn a_content_type_that_is_not_a_media_type_is_rejected() {
        for content_type in ["html", "", "text/html\\r\\nx-injected: 1"] {
            let response = format!("      content_type: \"{content_type}\"\n");
            let error = with_plugins(&entry(&response))
                .expect_err("a content type is a `type/subtype` pair");
            assert!(
                matches!(
                    error,
                    DocumentError::InvalidField {
                        field: "plugins[].error_response.content_type",
                        ..
                    }
                ),
                "expected InvalidField for content type {content_type:?}, got {error:?}"
            );
        }
    }

    #[test]
    fn a_parameterized_content_type_is_accepted() {
        let response = "      content_type: text/html; charset=utf-8\n      body: '<h1>no</h1>'\n";
        let document = with_plugins(&entry(response)).expect("a parameterized media type is valid");

        assert_eq!(
            document.plugins()[0]
                .error_response()
                .map(ErrorResponse::content_type),
            Some("text/html; charset=utf-8")
        );
    }
}
