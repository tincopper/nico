//! Tests for the compiled refusal: what a document can configure, what it cannot express, and what
//! the data plane is handed.
//!
//! The wire-level header set belongs to `nico-http`; what is asserted here is the value a filter
//! produces — status, stable code, challenge, category and the bytes of the body — and the fact that
//! every rule is applied while the document is compiled rather than per request.

use nico_core::ErrorCategory;
use nico_http::{HttpError, Rejection};

use crate::{
    PluginError,
    api_key::ApiKeyConfig,
    jwt::JwtConfig,
    rate_limit::RateLimitConfig,
    rejection::AuthRejections,
    spec::{ApiKeySpec, JwtSpec, RateLimitSpec},
    test_support::{document, plugin, refused},
};

/// A `jwt` entry, spliced under `plugins:`.
fn jwt_entry(error_response: &str) -> String {
    format!(
        concat!(
            "  - name: jwt\n",
            "    kind: jwt\n",
            "    config:\n",
            "      keys:\n",
            "        - kid: k1\n",
            "          secret_ref: env:JWT_SECRET\n",
            "{}",
        ),
        error_response
    )
}

/// A `rate_limit` entry, spliced under `plugins:`.
fn rate_limit_entry(error_response: &str) -> String {
    format!(
        concat!(
            "  - name: quota\n",
            "    kind: rate_limit\n",
            "    config:\n",
            "      burst: 2\n",
            "      per_second: 1\n",
            "{}",
        ),
        error_response
    )
}

/// Compiles a `jwt` entry's two refusals.
fn auth_refusals(error_response: &str) -> AuthRejections {
    let document = document(&jwt_entry(error_response), &["jwt"]);
    let plugin = plugin(&document, "jwt");
    let config = JwtConfig::from_spec(plugin).expect("the fixture configuration compiles");
    JwtSpec::compile(plugin, &config)
        .expect("the fixture compiles")
        .rejections
}

/// Compiles a `rate_limit` entry's refusal.
fn limit_refusal(error_response: &str) -> Rejection {
    let document = document(&rate_limit_entry(error_response), &["quota"]);
    let plugin = plugin(&document, "quota");
    let config = RateLimitConfig::from_spec(plugin).expect("the fixture configuration compiles");
    RateLimitSpec::compile(plugin, &config)
        .expect("the fixture compiles")
        .rejection
        .as_ref()
        .clone()
}

/// Compiles a `jwt` entry, expecting the refusal presentation to be refused.
fn auth_error(error_response: &str) -> PluginError {
    let document = document(&jwt_entry(error_response), &["jwt"]);
    let plugin = plugin(&document, "jwt");
    let config = JwtConfig::from_spec(plugin).expect("the fixture configuration compiles");
    JwtSpec::compile(plugin, &config)
        .err()
        .expect("the fixture must not compile")
}

/// Renders one refusal's body for `trace`.
fn body(rejection: &Rejection, trace: &str) -> String {
    String::from_utf8(rejection.render(trace).to_vec()).expect("a body is UTF-8")
}

mod defaults {
    use super::*;

    #[test]
    fn a_built_in_authentication_refusal_is_401_with_both_stable_codes_and_a_challenge() {
        let refusals = auth_refusals("");

        for (rejection, code) in [
            (refusals.missing(), "auth_missing"),
            (refusals.invalid(), "auth_invalid"),
        ] {
            assert_eq!(rejection.status().as_u16(), 401);
            assert_eq!(rejection.code(), code);
            assert_eq!(
                rejection.challenge(),
                Some("Bearer"),
                "a 401 tells the caller how to authenticate"
            );
            assert_eq!(rejection.category(), ErrorCategory::Client);
            assert_eq!(rejection.content_type(), "application/json");
        }
    }

    #[test]
    fn the_built_in_body_is_the_documented_envelope() {
        let refusals = auth_refusals("");

        assert_eq!(
            body(&refusals.invalid(), "trace-1"),
            r#"{"error":{"code":"auth_invalid","message":"the credential is not accepted","trace_id":"trace-1"}}"#
        );
        assert_eq!(
            body(&refusals.missing(), "trace-1"),
            r#"{"error":{"code":"auth_missing","message":"no credential was presented","trace_id":"trace-1"}}"#
        );
    }

    #[test]
    fn a_built_in_rate_limit_refusal_is_429_a_limit_and_offers_no_challenge() {
        let rejection = limit_refusal("");

        assert_eq!(rejection.status().as_u16(), 429);
        assert_eq!(rejection.code(), "rate_limited");
        assert_eq!(rejection.category(), ErrorCategory::Limit);
        assert_eq!(
            rejection.challenge(),
            None,
            "a limit must not tell the caller to authenticate"
        );
    }

    #[test]
    fn a_rate_limit_reached_through_a_document_still_reports_the_limit_category() {
        // The category travels with the plugin, not with the status: a limit published as a 403
        // would still be logged as a limit, and a monitor would still separate it from a refusal.
        let rejection = limit_refusal("    error_response:\n      status: 403\n");

        assert_eq!(rejection.status().as_u16(), 403);
        assert_eq!(rejection.category(), ErrorCategory::Limit);
    }
}

mod custom_presentations {
    use super::*;

    #[test]
    fn a_configured_status_and_json_body_replace_the_built_in_presentation() {
        let refusals = auth_refusals(concat!(
            "    error_response:\n",
            "      status: 403\n",
            "      body:\n",
            "        denied:\n",
            "          code: ${code}\n",
            "          hint: ask an operator\n",
            "          trace: ${trace_id}\n",
        ));

        let rejection = refusals.invalid();
        assert_eq!(rejection.status().as_u16(), 403);
        assert_eq!(
            rejection.code(),
            "auth_invalid",
            "the stable code is not configurable"
        );
        assert_eq!(
            body(&rejection, "trace-2"),
            r#"{"denied":{"code":"auth_invalid","hint":"ask an operator","trace":"trace-2"}}"#
        );
    }

    #[test]
    fn a_text_body_is_a_template_and_its_variables_are_written_verbatim() {
        let refusals = auth_refusals(concat!(
            "    error_response:\n",
            "      content_type: text/html\n",
            "      body: <h1>${code}</h1><p>${message}</p>\n",
        ));

        let rejection = refusals.missing();
        assert_eq!(rejection.content_type(), "text/html");
        assert_eq!(
            body(&rejection, "trace-3"),
            "<h1>auth_missing</h1><p>no credential was presented</p>"
        );
    }

    #[test]
    fn a_json_literal_is_escaped_while_the_template_syntax_is_not() {
        // The literal text of a JSON body comes from a document, so it may contain a quote or a
        // newline. The compiled template writes valid JSON syntax around it.
        let refusals = auth_refusals(concat!(
            "    error_response:\n",
            "      body:\n",
            "        note: a \"quoted\" line\\nnext\n",
            "        code: ${code}\n",
        ));

        let rendered = body(&refusals.invalid(), "trace-4");
        assert_eq!(
            rendered, r#"{"note":"a \"quoted\" line\\nnext","code":"auth_invalid"}"#,
            "the document's text is escaped, its structure is preserved"
        );
    }

    #[test]
    fn a_json_variable_is_a_string_value_so_it_can_never_inject_structure() {
        let refusals = auth_refusals(concat!(
            "    error_response:\n",
            "      body:\n",
            "        count: ${code}\n",
        ));

        let rendered = body(&refusals.invalid(), "trace-5");
        assert_eq!(
            rendered, r#"{"count":"auth_invalid"}"#,
            "a variable fills a string slot; it cannot become an object or a number"
        );
    }

    #[test]
    fn a_trace_id_from_the_request_is_escaped_in_a_custom_json_body() {
        let refusals = auth_refusals(concat!(
            "    error_response:\n",
            "      body:\n",
            "        trace: ${trace_id}\n",
        ));

        let rendered = body(&refusals.invalid(), "a\"b\\c\nd\u{1}");
        assert_eq!(
            rendered, r#"{"trace":"a\"b\\c\nd\u0001"}"#,
            "a caller-controlled value must not be able to end the string it is written into"
        );
    }

    #[test]
    fn a_body_can_repeat_the_same_variable() {
        let refusals = auth_refusals(concat!(
            "    error_response:\n",
            "      content_type: text/plain\n",
            "      body: ${code} ${code}\n",
        ));

        assert_eq!(body(&refusals.invalid(), ""), "auth_invalid auth_invalid");
    }
}

mod refused_presentations {
    use super::*;

    #[test]
    fn an_unknown_variable_is_refused_and_the_plugin_is_named() {
        let error = auth_error(concat!(
            "    error_response:\n",
            "      content_type: text/plain\n",
            "      body: ${secret} is not a variable\n",
        ));

        match &error {
            PluginError::UnknownVariable { plugin, variable } => {
                assert_eq!(plugin, "jwt");
                assert_eq!(variable, "secret");
            }
            other => panic!("expected UnknownVariable, got {other:?}"),
        }
        assert_eq!(error.category(), ErrorCategory::Client);
        assert!(
            error.to_string().contains("`jwt`"),
            "the message must name the plugin: {error}"
        );
    }

    #[test]
    fn an_unclosed_variable_is_refused() {
        let error = auth_error(concat!(
            "    error_response:\n",
            "      content_type: text/plain\n",
            "      body: code is ${code and more\n",
        ));

        assert!(
            matches!(error, PluginError::UnterminatedTemplate { ref plugin } if plugin == "jwt"),
            "expected UnterminatedTemplate, got {error:?}"
        );
    }

    #[test]
    fn an_unknown_variable_name_is_reported_without_its_control_characters() {
        let error = auth_error(concat!(
            "    error_response:\n",
            "      content_type: text/plain\n",
            "      body: \"${a\\tb}\"\n",
        ));

        let rendered = error.to_string();
        assert!(
            !rendered.contains('\t'),
            "a document must not be able to forge a log line: {rendered:?}"
        );
        assert!(
            rendered.contains("a?b"),
            "the name is still identifiable: {rendered}"
        );
    }

    #[test]
    fn a_template_over_the_literal_bound_is_refused() {
        // One string longer than the whole template budget: the limit is on compiled literals, so it
        // cannot be dodged by splitting the text into more parts.
        let literal = "x".repeat(5 * 1024);
        let error = auth_error(&format!(
            concat!(
                "    error_response:\n",
                "      content_type: text/plain\n",
                "      body: {literal}\n",
            ),
            literal = literal
        ));

        assert!(
            matches!(error, PluginError::Rejection { ref plugin, .. } if plugin == "jwt"),
            "expected a Rejection failure, got {error:?}"
        );
        assert!(
            error.to_string().contains("template"),
            "the message must name the rule: {error}"
        );
    }

    #[test]
    fn a_non_json_content_type_without_a_body_is_refused() {
        // The built-in envelope is JSON, so there is nothing to write under another content type.
        let error = auth_error("    error_response:\n      content_type: text/html\n");

        assert!(
            matches!(error, PluginError::ContentTypeWithoutBody { ref plugin } if plugin == "jwt"),
            "expected ContentTypeWithoutBody, got {error:?}"
        );
    }

    #[test]
    fn the_document_model_refuses_a_status_outside_4xx_before_the_plugin_sees_it() {
        // The rule is shared: the shape is checked by the document model, and the compiled
        // presentation is checked again by the plugin. Both refuse a 5xx, because a rejection the
        // caller can fix is never a gateway failure.
        let error = refused(&jwt_entry(
            "    error_response:\n      status: 500\n      body:\n        code: ${code}\n",
        ));

        assert!(
            error.to_string().contains("status"),
            "the message must name the field: {error}"
        );
    }

    #[test]
    fn an_api_key_entry_compiles_the_same_presentation() {
        let document = document(
            concat!(
                "  - name: partner\n",
                "    kind: api_key\n",
                "    config:\n",
                "      keys:\n",
                "        - name: acme\n",
                "          secret_ref: env:ACME_KEY\n",
                "    error_response:\n",
                "      status: 403\n",
                "      body:\n",
                "        code: ${code}\n",
            ),
            &["partner"],
        );
        let plugin = plugin(&document, "partner");
        let config = ApiKeyConfig::from_spec(plugin).expect("the fixture configuration compiles");
        let spec = ApiKeySpec::compile(plugin, &config).expect("the fixture compiles");

        assert_eq!(spec.rejections.invalid().status().as_u16(), 403);
        assert_eq!(spec.rejections.invalid().challenge(), Some("Bearer"));
    }
}

mod wire_contract {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn a_compiled_rejection_becomes_one_respond_without_a_second_mapping() {
        let rejection = Arc::new(limit_refusal(""));
        let respond = HttpError::Rejected {
            rejection: Arc::clone(&rejection),
            detail: "the caller is over its rate limit",
            retry_after: Some(std::time::Duration::from_millis(250)),
        }
        .respond()
        .expect("a rejection is renderable");

        assert_eq!(respond.status(), rejection.status());
        assert_eq!(respond.code(), rejection.code());
        assert_eq!(respond.content_type(), rejection.content_type());
        assert_eq!(respond.challenge(), None);
        assert_eq!(
            respond.retry_after(),
            Some(std::time::Duration::from_millis(250)),
            "the writer rounds the wait up, so it must receive the real one"
        );
    }

    #[test]
    fn a_rejection_carries_its_category_independently_of_the_status_it_renders() {
        let rejection = Arc::new(limit_refusal("    error_response:\n      status: 400\n"));
        let error = HttpError::Rejected {
            rejection,
            detail: "the caller is over its rate limit",
            retry_after: None,
        };

        assert_eq!(error.category(), ErrorCategory::Limit);
        assert_eq!(
            error.respond().map(|respond| respond.status().as_u16()),
            Some(400)
        );
    }
}
