//! The closed status table: what a provider failure status means without its body.
//!
//! The table is asserted twice: once row by row, so a change to it is visible as a change to a
//! documented status, and once against the body-reading path, so the two ways a failure status is
//! mapped cannot drift apart.

use nico_core::ErrorCategory;
use nico_llm::{Provider, provider::openai::OpenAiProvider};

use crate::support::{facts, provider};

/// One row of the table: provider status, category, code, caller status, retryable.
type Row = (u16, ErrorCategory, &'static str, u16, bool);

#[test]
fn the_status_table_maps_every_failure_without_reading_a_body() {
    let cases: [Row; 11] = [
        (400, ErrorCategory::Client, "upstream_rejected", 400, false),
        (
            401,
            ErrorCategory::Upstream,
            "upstream_auth_failed",
            502,
            false,
        ),
        (
            403,
            ErrorCategory::Upstream,
            "upstream_auth_failed",
            502,
            false,
        ),
        (
            404,
            ErrorCategory::Client,
            "upstream_model_not_found",
            404,
            false,
        ),
        (408, ErrorCategory::Upstream, "upstream_timeout", 504, true),
        (
            429,
            ErrorCategory::Limit,
            "upstream_rate_limited",
            429,
            true,
        ),
        (418, ErrorCategory::Client, "upstream_rejected", 418, false),
        (500, ErrorCategory::Upstream, "upstream_error", 502, true),
        (503, ErrorCategory::Upstream, "upstream_error", 502, true),
        (302, ErrorCategory::Upstream, "upstream_error", 502, false),
        (199, ErrorCategory::Upstream, "upstream_error", 502, false),
    ];

    for (provider_status, category, code, caller_status, retryable) in cases {
        let error = provider().error_for_status(provider_status);
        error.validate().unwrap_or_else(|failure| {
            panic!("{provider_status} maps to an invalid error: {failure}")
        });
        assert_eq!(error.category(), category, "category for {provider_status}");
        assert_eq!(error.code(), code, "code for {provider_status}");
        assert_eq!(
            error.http_status(),
            caller_status,
            "caller status for {provider_status}"
        );
        assert_eq!(
            error.retryable(),
            retryable,
            "retryability for {provider_status}"
        );
        assert!(
            error.provider_code().is_none(),
            "the provider's own code is only in its body and is never guessed"
        );
    }
}

#[test]
fn the_two_status_paths_agree_about_a_failure() {
    // `head` writes the client status before the body is read; `decode_response` reads the body
    // afterwards. A status answered differently by the two would put a body under a head that
    // disagrees with it.
    for status in [400, 401, 403, 404, 408, 418, 429, 500, 503] {
        let declared = provider().error_for_status(status);
        let decoded = provider()
            .decode_response(&facts(status), br#"{"error":{"code":"provider_marker"}}"#)
            .expect_err("a failure status is not a response");
        let nico_llm::ProviderError::Upstream { error } = decoded else {
            panic!("{status} must be an upstream failure: {decoded:?}");
        };
        assert_eq!(declared.category(), error.category(), "{status}");
        assert_eq!(declared.code(), error.code(), "{status}");
        assert_eq!(declared.http_status(), error.http_status(), "{status}");
        assert_eq!(declared.retryable(), error.retryable(), "{status}");
    }
}

#[test]
fn the_adapter_maps_the_status_it_is_asked_about() {
    // The trait method is the adapter's, not a wrapper's: a second adapter declares its own table.
    let adapter: &dyn Provider = &OpenAiProvider::new();
    assert_eq!(
        adapter.error_for_status(429).code(),
        provider().error_for_status(429).code()
    );
}

#[test]
fn the_adapter_names_the_protocol_it_speaks() {
    let adapter = OpenAiProvider::new();
    assert_eq!(
        adapter.native_protocol().as_str(),
        "openai.chat",
        "publication compares this with a passthrough route's client protocol"
    );
    assert_eq!(
        adapter.native_protocol(),
        nico_llm::provider::openai::NATIVE_PROTOCOL
    );
}

#[test]
fn the_exchange_answers_a_status_without_a_body() {
    // The transform needs the failure before any body byte exists, so the exchange exposes the same
    // table the response path uses.
    use std::sync::Arc;

    use nico_llm::Exchange;

    use crate::support::{codec, route};

    let exchange = Exchange::try_new(
        Arc::new(codec()),
        Arc::new(OpenAiProvider::new()),
        Arc::new(route("http://127.0.0.1:8080")),
    )
    .expect("the pair builds");
    let error = exchange.error_for_status(503);

    assert_eq!(error.code(), "upstream_error");
    assert_eq!(error.http_status(), 502);
}
