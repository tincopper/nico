//! Tests for the plan a document compiles to: the global chain, interning and the framing decisions.
//!
//! The subject is the whole plan rather than one plugin: which entries run before routing, which
//! entries a route shares with that chain, and which routes buffer the request body or lose the
//! upstream's response length. Each fixture is a real document parsed by `Document::parse`, so a test
//! can only prove something about a shape an operator could actually write.

use std::collections::{HashMap, HashSet};

use nico_core::ErrorCategory;
use nico_http::PluginPhases;
use tempfile::TempDir;

use super::{
    super::{PluginError, order, plan as compile_plan},
    PluginRuntime, QUOTA_ENTRY, SECRET, api_key_entry, compiled, document_with,
    document_with_globals, jwt_entry, route, secret_file,
};

/// A `rate_limit` entry named `quota` that asks for the whole request body.
const BUFFERED_QUOTA_ENTRY: &str = concat!(
    "  - name: quota\n",
    "    kind: rate_limit\n",
    "    buffers_body: true\n",
    "    config:\n",
    "      burst: 5\n",
    "      per_second: 2\n",
);

#[test]
fn the_global_chain_runs_the_listed_entries_in_phase_order() {
    // Written limit first, then jwt, then api_key: the phases sort both authentication kinds ahead of
    // the limit, and keep the order of `global_plugins` within a phase.
    let directory = TempDir::new().expect("temporary directory");
    let secret = secret_file(&directory, SECRET);
    let document = document_with_globals(
        &format!(
            "{}{}{}",
            QUOTA_ENTRY,
            jwt_entry(&secret),
            api_key_entry(&secret)
        ),
        "quota, jwt, api-key",
        &route("open", &[]),
    );

    let plan = PluginRuntime::new()
        .compile(&document)
        .expect("the fixture compiles");

    assert_eq!(
        format!("{:?}", plan.global()),
        r#"["jwt", "api-key", "quota"]"#,
        "the global chain holds the listed entries, authentication before the limit"
    );
}

#[test]
fn a_disabled_entry_runs_nowhere_but_a_route_may_still_name_it() {
    // The credential names a file that does not exist: compiling the entry at all would fail, so this
    // also proves the entry is never built.
    let document = document_with_globals(
        concat!(
            "  - name: jwt\n",
            "    kind: jwt\n",
            "    enabled: false\n",
            "    config:\n",
            "      keys:\n",
            "        - kid: k1\n",
            "          secret_ref: file:/nonexistent/nico/disabled-secret\n",
        ),
        "jwt",
        &route("api", &["jwt"]),
    );

    let plan = PluginRuntime::new()
        .compile(&document)
        .expect("a disabled entry compiles to nothing");

    assert_eq!(
        format!("{:?}", plan.global()),
        "[]",
        "a disabled entry runs before routing for nobody"
    );
    assert_eq!(
        format!(
            "{:?}",
            plan.routes()
                .chain_for("api")
                .expect("a route that names a disabled plugin still resolves to a chain")
        ),
        "[]",
        "the disabled entry contributes no filter, and the route is not left without a chain"
    );
}

#[test]
fn an_unknown_kind_that_is_disabled_is_skipped_rather_than_refused() {
    // Disabling is an operator's explicit "do not load this", so it is checked before the registry:
    // otherwise a document could not be stopped from failing publication by the very entry it stopped.
    let document = document_with(
        "  - name: wasm-policy\n    kind: wasm\n    enabled: false\n",
        &route("api", &["wasm-policy"]),
    );

    let plan = PluginRuntime::new()
        .compile(&document)
        .expect("a disabled entry is not looked up");

    assert_eq!(
        format!(
            "{:?}",
            plan.routes().chain_for("api").expect("the route resolves")
        ),
        "[]"
    );
}

#[test]
fn authentication_runs_before_the_limit_whatever_order_the_route_lists() {
    // The route writes the limit first. Order is the *kind's* decision, so authentication still runs
    // first: an unauthenticated request must not spend a token of someone else's quota.
    let directory = TempDir::new().expect("temporary directory");
    let secret = secret_file(&directory, SECRET);
    let document = document_with(
        &format!("{}{}", jwt_entry(&secret), QUOTA_ENTRY),
        &route("api", &["quota", "jwt"]),
    );

    let plan = PluginRuntime::new()
        .compile(&document)
        .expect("the fixture compiles");

    assert_eq!(
        format!(
            "{:?}",
            plan.routes()
                .chain_for("api")
                .expect("the route has a chain")
        ),
        r#"["jwt", "quota"]"#,
        "the phase decides the order, not the document"
    );
}

#[test]
fn every_kind_runs_in_the_phase_its_family_defines() {
    let directory = TempDir::new().expect("temporary directory");
    let secret = secret_file(&directory, SECRET);
    let document = document_with(
        &format!(
            "{}{}{}",
            jwt_entry(&secret),
            api_key_entry(&secret),
            QUOTA_ENTRY
        ),
        // Written limit first, then api_key, then jwt: the phases sort them back.
        &route("api", &["quota", "api-key", "jwt"]),
    );

    let plan = PluginRuntime::new()
        .compile(&document)
        .expect("the fixture compiles");

    assert_eq!(
        format!(
            "{:?}",
            plan.routes()
                .chain_for("api")
                .expect("the route has a chain")
        ),
        r#"["api-key", "jwt", "quota"]"#,
        "both authentication kinds precede the limit and keep their document order (api-key first)"
    );
}

#[test]
fn routes_that_resolve_to_the_same_filters_share_one_chain() {
    let directory = TempDir::new().expect("temporary directory");
    let secret = secret_file(&directory, SECRET);
    let routes = format!(
        "{}{}{}",
        route("one", &["jwt", "quota"]),
        route("two", &["jwt", "quota"]),
        // The same two plugins written the other way round: the phases make this the same chain, so
        // interning keys on what a route resolves to rather than on the text it wrote.
        route("three", &["quota", "jwt"]),
    );
    let document = document_with_globals(
        &format!("{}{}", jwt_entry(&secret), QUOTA_ENTRY),
        "jwt, quota",
        &routes,
    );
    let compiled = compiled(&document);

    let plan =
        compile_plan(&document, &compiled, &HashSet::new()).expect("the fixture plan is complete");

    assert_eq!(
        plan.routes.len(),
        3,
        "every route that names a plugin has a chain"
    );
    assert_eq!(
        plan.chains, 1,
        "three routes with one policy set allocate one chain, and the global chain is that same set"
    );
}

#[test]
fn a_route_that_names_no_plugin_has_no_chain() {
    let document = document_with(QUOTA_ENTRY, &route("open", &[]));

    let plan = PluginRuntime::new()
        .compile(&document)
        .expect("a document without policy references compiles");

    assert!(
        plan.routes().chain_for("open").is_none(),
        "a route with no plugins takes the zero-cost path"
    );
}

#[test]
fn a_route_whose_plugin_no_compiled_entry_covers_is_refused() {
    // The document model rejects a dangling reference, so the compiler checks its *own* coverage: a
    // route that ended up without a chain would run with no policy at all.
    let error = order(
        "api",
        &["jwt".to_owned()],
        &HashMap::new(),
        &[],
        &HashSet::new(),
    )
    .expect_err("a missing entry must refuse the plan");

    assert!(
        matches!(
            &error,
            PluginError::MissingChain { route, plugin } if route == "api" && plugin == "jwt"
        ),
        "expected MissingChain for api/jwt, got {error:?}"
    );
    assert_eq!(
        error.category(),
        ErrorCategory::Internal,
        "an incomplete plan is this crate's defect, not the operator's"
    );
}

#[test]
fn a_route_that_declares_a_body_buffering_entry_is_marked() {
    // Two routes name the same buffering entry and therefore share one chain; a third names nothing.
    // The mark is per route, so it follows what a route names rather than what the filter is.
    let document = document_with(
        BUFFERED_QUOTA_ENTRY,
        &format!(
            "{}{}{}",
            route("api", &["quota"]),
            route("mirror", &["quota"]),
            route("open", &[])
        ),
    );

    let plan = PluginRuntime::new()
        .compile(&document)
        .expect("the fixture compiles");

    assert!(plan.buffered().contains("api"));
    assert!(
        plan.buffered().contains("mirror"),
        "two routes that share one chain are both marked"
    );
    assert!(
        !plan.buffered().contains("open"),
        "a route that names nothing streams"
    );
    assert_eq!(plan.buffered().len(), 2);
}

#[test]
fn a_global_entry_that_buffers_or_rewrites_marks_every_route() {
    // No built-in kind declares `RESPONSE_BODY`, so the declaration is injected on the compiled
    // entry — the only input the framing decision reads. A global entry runs on every request, so a
    // route that names no plugin at all still needs the buffered mode and a chunked response.
    let directory = TempDir::new().expect("temporary directory");
    let secret = secret_file(&directory, SECRET);
    let document = document_with_globals(
        &jwt_entry(&secret),
        "jwt",
        &format!("{}{}", route("api", &[]), route("open", &[])),
    );
    let mut entries = compiled(&document);
    for entry in &mut entries {
        entry.phases = PluginPhases::RESPONSE_BODY;
        entry.buffers_body = true;
    }

    let plan =
        compile_plan(&document, &entries, &HashSet::new()).expect("the fixture plan is complete");

    for route in ["api", "open"] {
        assert!(
            plan.buffered.contains(route),
            "{route} buffers its request body"
        );
        assert!(
            plan.rewriting_responses.contains(route),
            "{route} cannot keep a length-framed response"
        );
    }
}

#[test]
fn a_route_whose_entries_never_ask_for_the_body_is_not_marked() {
    let document = document_with(QUOTA_ENTRY, &route("api", &["quota"]));

    let plan = PluginRuntime::new()
        .compile(&document)
        .expect("the fixture compiles");

    assert!(
        plan.buffered().is_empty(),
        "without a declared body mode the request streams"
    );
}

#[test]
fn a_plugin_declared_and_named_by_a_route_is_one_instance_in_both_chains() {
    // The entry is declared once and a route names it, so the global chain and the route chain must
    // drive the same plugin instance: a second element would keep a second piece of runtime
    // state (a token bucket, say) for one configured plugin, and the two would drift apart.
    let directory = TempDir::new().expect("temporary directory");
    let secret = secret_file(&directory, SECRET);
    let document = document_with_globals(
        &format!("{}{}", jwt_entry(&secret), QUOTA_ENTRY),
        "jwt, quota",
        &format!(
            "{}{}",
            route("api", &["jwt", "quota"]),
            route("limited", &["quota"])
        ),
    );
    let compiled = compiled(&document);

    let plan =
        compile_plan(&document, &compiled, &HashSet::new()).expect("the fixture plan is complete");

    let api = plan.routes.get("api").expect("the route has a chain");
    assert_eq!(
        api.len(),
        plan.global.len(),
        "the route names every declared entry"
    );
    for (global, routed) in plan.global.iter().zip(api.iter()) {
        assert!(
            global.is_same_instance(routed),
            "a filter the route shares with the global chain is one instance"
        );
    }
    let limited = plan.routes.get("limited").expect("the route has a chain");
    assert_eq!(limited.len(), 1);
    assert!(
        plan.global[1].is_same_instance(&limited[0]),
        "and a route that names only the limit holds the instance the global chain runs"
    );
}

#[test]
fn a_declared_entry_that_is_not_listed_globally_runs_only_where_a_route_names_it() {
    // The document declares the policy and a route uses it; nothing says it runs before routing, so
    // the global chain stays empty. This is the difference between a definition and a use site.
    let document = document_with(QUOTA_ENTRY, &route("api", &["quota"]));

    let plan = PluginRuntime::new()
        .compile(&document)
        .expect("the fixture compiles");

    assert_eq!(
        format!("{:?}", plan.global()),
        "[]",
        "declaring a plugin is not a decision to run it for every request"
    );
    assert_eq!(
        format!(
            "{:?}",
            plan.routes()
                .chain_for("api")
                .expect("the route names the declared entry")
        ),
        r#"["quota"]"#,
        "the route chain still holds it"
    );
}
