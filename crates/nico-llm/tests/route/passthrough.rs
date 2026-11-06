//! A passthrough route: what it publishes, and the four refusals that keep it legal.

use std::sync::Arc;

use nico_http::llm::LlmRoute;
use nico_llm::{
    ExchangeBuildError, ExchangeMode, ProtocolId, ProviderKind, ProviderRoute,
    protocol::openai_chat::PROTOCOL, provider::openai::PROVIDER_KIND, route::PassthroughRoute,
};

use crate::support::{BASE_URL, passthrough_route, provider, route};

/// Builds a route in passthrough mode.
fn passthrough_mode() -> ProviderRoute {
    route(BASE_URL).with_mode(ExchangeMode::Passthrough)
}

#[test]
fn the_route_reports_the_facts_the_upstream_head_needs() {
    let route = passthrough_route();

    assert!(
        route.converting().is_none(),
        "a passthrough route has no translation half to reach"
    );
    assert_eq!(route.mode_name(), "passthrough");
    assert_eq!(route.protocol(), "openai.chat");
    assert_eq!(route.provider(), "openai");
    assert_eq!(route.destination().host(), "127.0.0.1");
    assert_eq!(route.path(), "/v1/chat/completions");
    assert_eq!(
        route
            .headers()
            .iter()
            .find(|(name, _)| name == "authorization")
            .map(|(_, value)| value.as_str()),
        None,
        "a route without a credential presents none"
    );
}

#[test]
fn a_converting_route_has_no_passthrough_front_end() {
    let error = PassthroughRoute::try_new(Arc::new(route(BASE_URL)), &provider())
        .expect_err("the route converts");

    assert!(
        matches!(&error, ExchangeBuildError::NotPassthrough { route_id } if route_id == "chat"),
        "{error:?}"
    );
}

#[test]
fn a_provider_of_another_kind_is_refused() {
    let other = ProviderRoute::try_new("chat", PROTOCOL, ProviderKind::new("elsewhere"), BASE_URL)
        .expect("a valid route")
        .with_mode(ExchangeMode::Passthrough);
    let error = PassthroughRoute::try_new(Arc::new(other), &provider())
        .expect_err("the adapter serves another kind");

    assert!(
        matches!(
            &error,
            ExchangeBuildError::ProviderMismatch { provider, route }
                if *provider == PROVIDER_KIND && *route == ProviderKind::new("elsewhere")
        ),
        "{error:?}"
    );
}

#[test]
fn a_provider_that_speaks_another_protocol_is_refused() {
    // The client's bytes would reach a peer that parses something else, which is the silent
    // corruption the mode exists to prevent.
    let other = ProviderRoute::try_new(
        "chat",
        ProtocolId::new("elsewhere.chat"),
        PROVIDER_KIND,
        BASE_URL,
    )
    .expect("a valid route")
    .with_mode(ExchangeMode::Passthrough);
    let error = PassthroughRoute::try_new(Arc::new(other), &provider())
        .expect_err("the adapter does not speak this protocol");

    assert!(
        matches!(
            &error,
            ExchangeBuildError::NativeProtocolMismatch { route_id, provider, route }
                if route_id == "chat"
                    && *provider == ProtocolId::new("openai.chat")
                    && *route == ProtocolId::new("elsewhere.chat")
        ),
        "{error:?}"
    );
}

#[test]
fn an_unaddressable_base_url_is_a_build_failure() {
    let route = ProviderRoute::try_new("chat", PROTOCOL, PROVIDER_KIND, "ftp://api.example.test")
        .expect("a valid route")
        .with_mode(ExchangeMode::Passthrough);
    let error = PassthroughRoute::try_new(Arc::new(route), &provider())
        .expect_err("the adapter cannot address this route");

    assert!(
        matches!(error, ExchangeBuildError::Endpoint { .. }),
        "{error:?}"
    );
}

#[test]
fn a_passthrough_route_has_no_translation_half() {
    // The data plane forwards a passthrough route's bytes, and the mode is the type rather than a
    // flag: there is no `prepare` or `transform` on the value, so the illegal call has no spelling and
    // no request-time `500` exists for it.
    let route = passthrough_route();

    assert!(route.converting().is_none());
}

#[test]
fn the_route_debug_names_nothing_from_a_credential() {
    let route = PassthroughRoute::try_new(
        Arc::new(passthrough_mode().with_credential(nico_llm::Credential::new("sk-passthrough"))),
        &provider(),
    )
    .expect("the route and the adapter agree");
    let printed = format!("{route:?}");

    assert!(printed.contains("PassthroughRoute"), "{printed}");
    assert!(
        !printed.contains("sk-passthrough"),
        "the credential must not be printed: {printed}"
    );
}
