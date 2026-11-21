//! Tests for the published state: one value carrying a snapshot and the policies derived from it.
//!
//! The subject is the pairing, so every test asks for the parts of one generation at once. A test that
//! only checked the snapshot would pass even if the chains came from another publication — which is
//! exactly the fail-open combination `HttpState` exists to make impossible.

use std::sync::Arc;

use async_trait::async_trait;
use nico_core::{ConfigDigest, Generation, Revision, RuntimeSnapshot};

use super::{
    BufferedRoutes, HttpStateHandle, PublishedChains, ResponseRewrites, RouteChains,
    chain_for_route, llm_for_route,
};

/// Publishes a generation that serves no LLM route, which is what an upstream-only document compiles
/// to. The production path always has a plan; a test that wants the empty one says so.
fn publish(handle: &HttpStateHandle, snapshot: Arc<RuntimeSnapshot>, chains: PublishedChains) {
    handle.publish_with_llm(
        snapshot,
        chains,
        Arc::new(NoLlmRoutes),
        &ExpandedMembers::default(),
    );
}
use crate::{
    Plugin, PluginChain, PluginEntry, PluginInfo,
    error::HttpError,
    llm::NoLlmRoutes,
    test_support::{
        FakeLlmRoute, FakeLlmRoutes, LlmCalls, document, llm_context, llm_snapshot, route,
        snapshot, snapshot_with_plugins,
    },
    upstream::dns::ExpandedMembers,
};

/// A plugin with no behaviour: only its name matters here.
struct Noop;

#[async_trait]
impl Plugin for Noop {
    fn name(&self) -> &str {
        "noop"
    }

    fn info(&self) -> PluginInfo {
        PluginInfo::new("0.0.1", "does nothing", "{}")
    }
}

/// A chain holding one [`Noop`].
fn chain() -> PluginChain {
    PluginChain::new(
        vec![PluginEntry::new(
            Arc::new(Noop) as Arc<dyn Plugin>,
            Arc::from("{}"),
        )]
        .into(),
    )
}

/// A plan with one route chain per pair and the buffered route ids `buffered`, and no global chain.
fn plan(routes: &[(&str, PluginChain)], buffered: &[&str]) -> PublishedChains {
    PublishedChains::new(
        PluginChain::empty(),
        routes
            .iter()
            .map(|(route, chain)| ((*route).to_owned(), chain.clone()))
            .collect(),
        buffered.iter().map(|route| (*route).to_owned()).collect(),
        ResponseRewrites::empty(),
    )
}

/// A plan that runs [`chain`] before routing, on `api` after routing, and buffers `api`'s body.
fn full_plan() -> PublishedChains {
    PublishedChains::new(
        chain(),
        plan(&[("api", chain())], &[]).routes().clone(),
        BufferedRoutes::from_iter(["api".to_owned()]),
        ResponseRewrites::from_iter(["api".to_owned()]),
    )
}

/// Builds a snapshot of `routes` published as `generation`.
fn snapshot_at(generation: u64, routes: &str) -> Arc<RuntimeSnapshot> {
    Arc::new(RuntimeSnapshot::new(
        Generation::new(generation),
        Revision::comparable(generation),
        ConfigDigest::of(routes.as_bytes()),
        document(routes),
        "test",
    ))
}

/// Two routes sharing the fixture snapshot.
const TWO_ROUTES: &str = concat!(
    "  - id: api\n",
    "    path_prefix: /api/\n",
    "    upstream: api-backend\n",
    "  - id: open\n",
    "    path_prefix: /open/\n",
    "    upstream: api-backend\n",
);

/// A `rate_limit` entry a fixture route can name.
const POLICY: &str = concat!(
    "  - name: quota\n",
    "    kind: rate_limit\n",
    "    config:\n",
    "      burst: 2\n",
    "      per_second: 1\n",
);

/// One route that names [`POLICY`] and one that names nothing.
const POLICY_ROUTES: &str = concat!(
    "  - id: api\n",
    "    path_prefix: /api/\n",
    "    upstream: api-backend\n",
    "    plugins: [quota]\n",
    "  - id: open\n",
    "    path_prefix: /open/\n",
    "    upstream: api-backend\n",
);

#[test]
fn a_route_without_plugins_runs_no_chain() {
    let handle = HttpStateHandle::new();
    publish(
        &handle,
        snapshot_with_plugins(POLICY, POLICY_ROUTES),
        PublishedChains::empty(),
    );
    let state = handle.current().expect("the state is published");

    let chain = chain_for_route(&state, route(state.snapshot().document(), "open"));

    assert!(
        matches!(chain, Ok(None)),
        "a route with no plugins takes the zero-cost path, got {chain:?}"
    );
}

#[test]
fn a_route_that_names_a_plugin_runs_the_published_chain() {
    let handle = HttpStateHandle::new();
    publish(
        &handle,
        snapshot_with_plugins(POLICY, POLICY_ROUTES),
        plan(&[("api", chain())], &[]),
    );
    let state = handle.current().expect("the state is published");

    let chain = chain_for_route(&state, route(state.snapshot().document(), "api"))
        .expect("the plan covers the route")
        .expect("the route names a plugin");

    assert_eq!(format!("{chain:?}"), r#"["noop"]"#);
}

#[test]
fn a_route_that_names_a_plugin_without_a_chain_is_refused() {
    // The compiler that produced this publication skipped the route. Forwarding the request would
    // serve it with no policy at all, so the answer is a 500 rather than "no policy".
    let handle = HttpStateHandle::new();
    publish(
        &handle,
        snapshot_with_plugins(POLICY, POLICY_ROUTES),
        PublishedChains::empty(),
    );
    let state = handle.current().expect("the state is published");

    let chain = chain_for_route(&state, route(state.snapshot().document(), "api"));

    assert!(
        matches!(chain, Err(HttpError::Internal { .. })),
        "an uncovered route must fail closed, got {chain:?}"
    );
}

#[test]
fn nothing_is_published_before_the_first_publish() {
    let handle = HttpStateHandle::new();

    assert!(handle.current().is_none());
}

#[test]
fn publish_exposes_every_part_of_the_generation_together() {
    let handle = HttpStateHandle::new();
    let published = snapshot(TWO_ROUTES);

    publish(&handle, Arc::clone(&published), full_plan());

    let state = handle.current().expect("the state is published");
    assert_eq!(state.snapshot().generation(), published.generation());
    assert_eq!(
        format!("{:?}", state.global()),
        r#"["noop"]"#,
        "the global chain published with the snapshot is the one visible with it"
    );
    assert_eq!(
        state.chains().len(),
        1,
        "the per-route chains published with the snapshot are the ones visible with it"
    );
    assert!(state.chains().chain_for("api").is_some());
    assert!(
        state.buffered().contains("api"),
        "a buffered route published with the snapshot is visible with it"
    );
    assert!(
        state.rewrites_response("api"),
        "and so is the route whose response body a plugin rewrites"
    );
    assert!(
        !state.buffered().contains("open"),
        "and a route that declared no body mode is not marked"
    );
}

#[test]
fn a_route_without_plugins_has_no_chain() {
    let chains = RouteChains::empty();

    assert!(chains.is_empty());
    assert!(
        chains.chain_for("api").is_none(),
        "a route with no plugins takes the zero-cost path"
    );
}

#[test]
fn a_chain_is_found_only_for_the_route_that_declared_it() {
    let chains: RouteChains = [("api".to_owned(), chain())].into_iter().collect();

    assert!(chains.chain_for("open").is_none());
    let found = chains.chain_for("api").expect("the route has a chain");
    assert_eq!(format!("{found:?}"), r#"["noop"]"#);
}

#[test]
fn two_routes_that_reference_the_same_plugins_share_one_chain() {
    let one = chain();
    // A `PluginChain` clone is one `Arc` bump: the pipeline interns a chain by cloning it, so two
    // routes with the same plugin list do not compile or allocate a second copy.
    let chains: RouteChains = [("api".to_owned(), one.clone()), ("open".to_owned(), one)]
        .into_iter()
        .collect();

    assert_eq!(chains.len(), 2);
    for route in ["api", "open"] {
        assert_eq!(
            format!(
                "{:?}",
                chains.chain_for(route).expect("the route has a chain")
            ),
            r#"["noop"]"#
        );
    }
}

#[test]
fn a_held_state_keeps_the_generation_and_the_policies_it_was_read_at() {
    let handle = HttpStateHandle::new();
    publish(&handle, snapshot_at(1, TWO_ROUTES), full_plan());
    let held = handle.current().expect("the first state is published");

    publish(
        &handle,
        snapshot_at(2, TWO_ROUTES),
        PublishedChains::empty(),
    );

    assert_eq!(
        held.snapshot().generation(),
        Generation::new(1),
        "a request that already read a state keeps its generation"
    );
    assert!(
        held.chains().chain_for("api").is_some(),
        "and the chain that generation compiled"
    );
    assert_eq!(
        format!("{:?}", held.global()),
        r#"["noop"]"#,
        "and the global chain, not the one the newer publication carries"
    );
    assert!(held.buffered().contains("api"));
    let current = handle.current().expect("the second state is published");
    assert_eq!(current.snapshot().generation(), Generation::new(2));
    assert!(
        current.chains().chain_for("api").is_none(),
        "the newer publication is a different generation with different policies"
    );
}

#[test]
fn a_new_publication_replaces_the_whole_plan() {
    let handle = HttpStateHandle::new();
    publish(&handle, snapshot(TWO_ROUTES), full_plan());
    publish(&handle, snapshot(TWO_ROUTES), PublishedChains::empty());

    let state = handle.current().expect("the state is published");
    assert!(
        state.chains().chain_for("api").is_none(),
        "a route that lost its plugins must not keep running the old chain"
    );
    assert_eq!(
        format!("{:?}", state.global()),
        "[]",
        "and a generation that declared no plugin must not keep the old global chain"
    );
    assert!(state.buffered().is_empty(), "nor the previous body mode");
    assert!(
        !state.rewrites_response("api"),
        "nor the previous response framing"
    );
}

/// A publication carrying one route's LLM front-end, as the compiler that builds the plan would.
fn llm_plan() -> Arc<dyn crate::llm::LlmRoutes> {
    let calls = Arc::new(std::sync::Mutex::new(LlmCalls::default()));
    FakeLlmRoutes::one("ai", Arc::new(FakeLlmRoute::new(&calls)))
}

#[test]
fn a_provider_route_without_a_published_front_end_is_refused() {
    let handle = HttpStateHandle::new();
    publish(&handle, llm_snapshot(), PublishedChains::empty());
    let state = handle.current().expect("the state is published");

    let error = llm_for_route(&state, route(state.snapshot().document(), "ai"))
        .err()
        .expect("a provider this crate cannot address is never guessed");
    assert!(
        matches!(&error, HttpError::ProviderNotConfigured { name } if name == "openai-primary"),
        "expected ProviderNotConfigured, got {error:?}"
    );
    assert_eq!(
        error.respond().map(|respond| respond.status().as_u16()),
        Some(502)
    );
}

#[test]
fn a_provider_route_is_served_by_the_front_end_its_generation_published() {
    let handle = HttpStateHandle::new();
    handle.publish_with_llm(
        llm_snapshot(),
        PublishedChains::empty(),
        llm_plan(),
        &ExpandedMembers::default(),
    );
    let state = handle.current().expect("the state is published");

    let front_end = llm_for_route(&state, route(state.snapshot().document(), "ai"))
        .expect("the plan covers the route")
        .expect("the route targets a provider");

    assert_eq!(front_end.protocol(), "openai.chat");
    assert_eq!(front_end.provider(), "openai");
}

#[test]
fn an_upstream_route_has_no_llm_front_end() {
    let handle = HttpStateHandle::new();
    publish(&handle, snapshot(TWO_ROUTES), PublishedChains::empty());
    let state = handle.current().expect("the state is published");

    let front_end = llm_for_route(&state, route(state.snapshot().document(), "api"))
        .expect("an upstream route is not refused");

    assert!(
        front_end.is_none(),
        "an upstream route forwards bytes and never consults the plan"
    );
}

#[test]
fn publish_stores_no_front_end() {
    let handle = HttpStateHandle::new();

    publish(&handle, llm_snapshot(), PublishedChains::empty());

    let state = handle.current().expect("the state is published");
    assert!(state.llm().is_empty());
    assert_eq!(state.llm().len(), 0);
    assert!(format!("{state:?}").contains("llm_routes: 0"));
}

#[test]
fn a_held_state_keeps_the_front_ends_its_generation_published() {
    let handle = HttpStateHandle::new();
    handle.publish_with_llm(
        llm_snapshot(),
        PublishedChains::empty(),
        llm_plan(),
        &ExpandedMembers::default(),
    );
    let held = handle.current().expect("the first state is published");

    handle.publish_with_llm(
        llm_snapshot(),
        PublishedChains::empty(),
        FakeLlmRoutes::empty(),
        &ExpandedMembers::default(),
    );

    assert!(
        held.llm().route("ai").is_some(),
        "a request that already read a state keeps the endpoint it was routed with"
    );
    assert!(
        held.llm()
            .route("ai")
            .is_some_and(|front_end| llm_context(held.snapshot(), front_end).llm().is_some()),
        "the front-end a held generation answers with is the one its routes were published for"
    );
    let current = handle.current().expect("the second state is published");
    assert!(
        current.llm().route("ai").is_none(),
        "the newer publication carries the plan compiled with it"
    );
}
