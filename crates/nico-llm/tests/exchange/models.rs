//! The model a route serves: the operator's allowlist, and where it is enforced.
//!
//! Split from `main.rs` by subject rather than by length: these tests are about the *model* half of a
//! route's facts, which is the only request-time policy besides the capability gate.

use std::{collections::BTreeMap, sync::Arc};

use nico_core::ErrorCategory;
use nico_llm::{
    Capabilities, Exchange, ExchangeError, ExchangeMode, ModelId, ProviderRoute, RouteError,
};

use super::mocks::{KIND, MockCodec, MockProvider, PROTOCOL, StreamBehaviour, body, request_facts};

/// Builds a route whose map hides the provider's name behind an alias, and whose allowlist names the
/// physical model it may reach.
fn allowlisted_route() -> Arc<ProviderRoute> {
    Arc::new(
        ProviderRoute::try_new("chat-route", PROTOCOL, KIND, "http://127.0.0.1:8080")
            .expect("a valid route")
            .with_mode(ExchangeMode::Convert)
            .with_model_map(BTreeMap::from([(
                "chat-small".to_owned(),
                "chat-small-2026-01-01".to_owned(),
            )]))
            .expect("a valid model map")
            .with_model_allowlist(vec!["chat-small-2026-01-01".to_owned()])
            .expect("a valid allowlist"),
    )
}

#[test]
fn the_allowlist_is_checked_on_the_resolved_model_not_the_callers_alias() {
    // The map exists so a caller never writes the provider's catalogue name, so the operator's list is
    // about what the *endpoint* may reach: the alias `chat-small` is not on it and is still served,
    // because it resolves to a name that is.
    let exchange = Exchange::try_new(
        MockCodec::new(Capabilities::all()),
        MockProvider::new(Capabilities::all(), StreamBehaviour::Lines),
        allowlisted_route(),
    )
    .expect("the mock pair matches the route");

    exchange
        .prepare(&request_facts(), &body("chat-small", "hello"))
        .expect("the alias resolves to a model the route serves");
}

#[test]
fn a_model_outside_the_allowlist_is_refused_before_the_provider_sees_it() {
    let exchange = Exchange::try_new(
        MockCodec::new(Capabilities::all()),
        MockProvider::new(Capabilities::all(), StreamBehaviour::Lines),
        allowlisted_route(),
    )
    .expect("the mock pair matches the route");

    let error = exchange
        .prepare(&request_facts(), &body("chat-large", "hello"))
        .expect_err("the route does not serve this model");
    assert_eq!(
        error,
        ExchangeError::ModelNotAllowed {
            model: ModelId::try_new("chat-large").expect("a valid name")
        }
    );
    let canonical = error.canonical();
    assert_eq!(canonical.http_status(), 403);
    assert_eq!(canonical.code(), "model_not_allowed");
    assert_eq!(canonical.category(), ErrorCategory::Client);
    assert!(
        !canonical.message().contains("chat-small"),
        "the answer never names the models the route does serve: {}",
        canonical.message()
    );
}

#[test]
fn an_allowlist_entry_must_be_a_model_name() {
    let error = ProviderRoute::try_new("chat-route", PROTOCOL, KIND, "http://127.0.0.1:8080")
        .expect("a valid route")
        .with_model_allowlist(vec!["   ".to_owned()])
        .expect_err("a blank name is not a model");

    assert!(
        matches!(error, RouteError::InvalidModel { .. }),
        "expected InvalidModel, got {error:?}"
    );
}
