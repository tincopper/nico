//! Tests for the plugin **envelope**: what an entry carries besides the kind's own configuration.
//!
//! Two subjects live here, because both belong to the document model rather than to any kind: the
//! envelope's own fields (the `kind` string, `enabled`, `buffers_body`, `config` passed through
//! untouched) and the rejection body conversion, which refuses a value no JSON body can represent
//! while the document is assembled — there is no later point at which it could be repaired.
//!
//! Range, name and reference rules live in `semantic` and `reference`. A kind's own field names and
//! ranges are not this crate's: they are covered where the kind is implemented.

use super::*;
use crate::{
    config::{Document, ParseLimits, error::DocumentError, test_support},
    error::ErrorCategory,
};

/// Parses a document whose single plugin entry is `entry`.
fn parse_plugin(entry: &str) -> Result<Document, DocumentError> {
    let plugins = format!("  - name: fixture\n{entry}");
    let document = test_support::document_with_plugins(
        test_support::NO_ENTRIES,
        test_support::NO_ENTRIES,
        test_support::NO_ENTRIES,
        &plugins,
    );
    Document::parse(document.as_bytes(), ParseLimits::new(16 * 1024))
}

/// A `jwt` entry holding one key, the minimum a kind needs to exist.
const JWT_ENTRY: &str = concat!(
    "    kind: jwt\n",
    "    config:\n",
    "      keys: [{ secret_ref: env:JWT_SECRET }]\n",
);

mod envelope {
    use super::*;

    #[test]
    fn the_kind_is_a_string_the_model_does_not_look_up() {
        // "A kind exists" is a registry question, answered when the document is compiled. If this
        // model had an opinion, adding a plugin would be a change to the kernel.
        let document = parse_plugin("    kind: something-new\n    config: { level: 3 }\n")
            .expect("an unknown kind is not a document error");

        assert_eq!(document.plugins()[0].kind(), "something-new");
    }

    #[test]
    fn the_kinds_configuration_is_carried_through_untouched() {
        let written = concat!(
            "    kind: whatever\n",
            "    config:\n",
            "      nested:\n",
            "        list: [1, two, true, null]\n",
            "      flag: false\n",
        );
        let document = parse_plugin(written).expect("the fixture is a valid document");
        let expected: serde_norway::Value =
            serde_norway::from_str("{ nested: { list: [1, two, true, null] }, flag: false }")
                .expect("the expected value parses");

        assert_eq!(
            document.plugins()[0].config(),
            &expected,
            "the value a document wrote is the value the kind's implementation reads"
        );
    }

    #[test]
    fn an_entry_without_configuration_carries_null() {
        let document = parse_plugin("    kind: something-new\n").expect("the fixture is valid");

        assert_eq!(
            document.plugins()[0].config(),
            &serde_norway::Value::Null,
            "`Value::Null` is how the owning crate sees \"no fields were set\""
        );
        assert!(document.plugins()[0].is_enabled());
        assert!(!document.plugins()[0].buffers_body());
    }

    #[test]
    fn enabled_defaults_to_true_and_can_be_turned_off() {
        // A disabled entry is still declared, so a route may keep referencing it: that is what makes
        // an emergency stop a one-line change instead of an edit to every route.
        let document = parse_plugin(&format!("    enabled: false\n{JWT_ENTRY}"))
            .expect("a disabled entry is a valid declaration");

        assert!(!document.plugins()[0].is_enabled());
    }

    #[test]
    fn buffers_body_defaults_to_false_and_can_be_requested() {
        let document = parse_plugin(&format!("    buffers_body: true\n{JWT_ENTRY}"))
            .expect("the fixture is a valid document");

        assert!(document.plugins()[0].buffers_body());
    }

    #[test]
    fn an_empty_kind_is_rejected_by_name() {
        for kind in ["\"\"", "\"   \""] {
            let error = parse_plugin(&format!("    kind: {kind}\n"))
                .expect_err("an entry that names no implementation can never compile");
            assert!(
                matches!(
                    error,
                    DocumentError::InvalidField {
                        field: "plugins[].kind",
                        ..
                    }
                ),
                "expected InvalidField for the kind, got {error:?}"
            );
            assert_eq!(error.category(), ErrorCategory::Client);
        }
    }

    #[test]
    fn a_field_the_envelope_does_not_have_is_rejected() {
        // The envelope is closed: everything a kind accepts goes under `config`, so a stray field
        // here can only be a typo. It is refused while the document is decoded.
        let error = parse_plugin(&format!("    module_ref: file:/p.wasm\n{JWT_ENTRY}"))
            .expect_err("`module_ref` is not an envelope field");

        assert!(
            matches!(error, DocumentError::Yaml { .. }),
            "expected a schema error, got {error:?}"
        );
        assert_eq!(error.category(), ErrorCategory::Client);
    }

    #[test]
    fn the_rejection_presentation_sits_on_the_envelope() {
        let document = parse_plugin(concat!(
            "    kind: jwt\n",
            "    error_response:\n",
            "      status: 403\n",
            "    config:\n",
            "      keys: [{ secret_ref: env:JWT_SECRET }]\n",
        ))
        .expect("the fixture is a valid document");

        assert_eq!(
            document.plugins()[0]
                .error_response()
                .and_then(ErrorResponse::status),
            Some(403)
        );
    }
}

mod rejection_body {
    use super::*;

    #[test]
    fn a_json_body_becomes_a_structured_value() {
        let document = parse_plugin(&format!(
            "{JWT_ENTRY}    error_response:\n      status: 403\n      body:\n        error:\n          code: \"{{code}}\"\n          retries: 2\n          nested: [1, true, null]\n"
        ))
        .expect("the fixture is a valid document");

        let response = document.plugins()[0]
            .error_response()
            .expect("the entry configures a rejection");
        assert_eq!(response.status(), Some(403));
        assert_eq!(response.content_type(), "application/json");
        let Some(ErrorBody::Json(BodyValue::Object(object))) = response.body() else {
            panic!("expected a JSON object body, got {:?}", response.body());
        };
        assert_eq!(object[0].0, "error");
        let BodyValue::Object(inner) = &object[0].1 else {
            panic!("expected a nested object");
        };
        assert_eq!(
            inner,
            &[
                ("code".to_owned(), BodyValue::String("{code}".to_owned())),
                ("retries".to_owned(), BodyValue::Number("2".to_owned())),
                (
                    "nested".to_owned(),
                    BodyValue::Array(vec![
                        BodyValue::Number("1".to_owned()),
                        BodyValue::Bool(true),
                        BodyValue::Null,
                    ])
                ),
            ]
        );
    }

    #[test]
    fn a_body_for_a_non_json_content_type_must_be_a_string() {
        let error = parse_plugin(&format!(
            "{JWT_ENTRY}    error_response:\n      content_type: text/html\n      body: 42\n"
        ))
        .expect_err("a number is not a text body");

        assert!(
            matches!(
                error,
                DocumentError::InvalidField {
                    field: "plugins[].error_response.body",
                    ..
                }
            ),
            "expected InvalidField for the body, got {error:?}"
        );
    }

    #[test]
    fn a_parameterized_json_content_type_still_selects_a_structured_body() {
        let document = parse_plugin(&format!(
            "{JWT_ENTRY}    error_response:\n      content_type: application/json; charset=utf-8\n      body: {{ error: denied }}\n"
        ))
        .expect("a JSON content type carries a structured body");
        let response = document.plugins()[0]
            .error_response()
            .expect("the entry configures a rejection");

        assert_eq!(
            response.content_type(),
            "application/json; charset=utf-8",
            "the parameter form is kept as written"
        );
        assert!(matches!(response.body(), Some(ErrorBody::Json(_))));
    }

    #[test]
    fn a_text_body_is_kept_verbatim() {
        let document = parse_plugin(&format!(
            "{JWT_ENTRY}    error_response:\n      content_type: text/html\n      body: '<h1>{{code}}</h1>'\n"
        ))
        .expect("a text body is a string");

        let response = document.plugins()[0]
            .error_response()
            .expect("the entry configures a rejection");
        assert_eq!(
            response.body(),
            Some(&ErrorBody::Text("<h1>{code}</h1>".to_owned())),
            "the text is stored as written; parsing its variables is the plugin crate's job"
        );
    }

    #[test]
    fn a_yaml_tag_in_a_body_is_rejected() {
        let error = parse_plugin(&format!(
            "{JWT_ENTRY}    error_response:\n      body: !custom value\n"
        ))
        .expect_err("a tagged value has no JSON spelling");

        assert_eq!(
            error.category(),
            ErrorCategory::Client,
            "the document is wrong, whatever stage refused it: {error:?}"
        );
    }

    #[test]
    fn an_infinite_number_is_rejected() {
        let error = parse_plugin(&format!(
            "{JWT_ENTRY}    error_response:\n      body: {{ ratio: .inf }}\n"
        ))
        .expect_err("an infinite number has no JSON spelling");

        assert!(
            matches!(error, DocumentError::InvalidField { ref reason, .. } if reason.contains("finite")),
            "expected a finite-number rejection, got {error:?}"
        );
    }

    #[test]
    fn an_object_key_that_is_not_a_string_is_rejected() {
        let error = parse_plugin(&format!(
            "{JWT_ENTRY}    error_response:\n      body: {{ 7: seven }}\n"
        ))
        .expect_err("a JSON object key is a string");

        assert!(
            matches!(error, DocumentError::InvalidField { ref reason, .. } if reason.contains("keys")),
            "expected an object-key rejection, got {error:?}"
        );
    }

    #[test]
    fn an_omitted_content_type_reads_the_body_as_json() {
        let document = parse_plugin(&format!(
            "{JWT_ENTRY}    error_response:\n      body: {{ denied: true }}\n"
        ))
        .expect("a structured body implies JSON");
        let response = document.plugins()[0]
            .error_response()
            .expect("the entry configures a rejection");

        assert_eq!(response.content_type(), DEFAULT_CONTENT_TYPE);
        assert!(matches!(response.body(), Some(ErrorBody::Json(_))));
    }
}
