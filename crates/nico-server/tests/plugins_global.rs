//! The global scope: the policies that run on every request, before routing has happened.
//!
//! Two statements are checked here, and both are about *order* rather than about a policy:
//!
//! - a global plugin runs before a route is matched, so it can refuse a request whose path matches
//!   nothing — the request a route-scoped policy never sees;
//! - within the request direction the global chain runs first and the route chain second, which is
//!   visible in the header both scopes append to.
//!
//! The document deliberately puts a tagging plugin *before* the authentication plugin in the same
//! global list. The `api_key` kind declares the `Authenticate` class, so it still runs first — the
//! list is sorted by class, not by the order it was written — and the tagger therefore records
//! nothing on the refused request even though it is listed first. The tagger is also the one whose
//! header the upstream sees, and it records that both scopes ran.
//!
//! One server, one document, one scenario: the three requests below are steps of one story, and a
//! second server in this binary would test Pingora's global runtime instead of the gateway.

mod support;

use std::{sync::Arc, time::Duration};

use support::{
    http::{
        RunningServer,
        client::{Request, exchange},
        free_port,
        mock::{Event, MockFactory, MockRecorder},
        resources::{Plugin, Resources, Route},
        source::TestSource,
        upstream::{MockUpstream, Reply},
        wait_for_status, write_bootstrap,
    },
    token,
};

/// The key the global `api_key` instance accepts.
const EDGE_KEY: &str = "the-edge-key-a-global-scenario-presents";

/// How long a wait driven by the mock's records may take.
const BUDGET: Duration = Duration::from_secs(10);

#[tokio::test(flavor = "current_thread")]
async fn a_global_plugin_runs_before_routing_and_before_the_route_scope() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();
    write_bootstrap(directory.path(), port, false, 1, 1000);
    let key_ref = token::secret_file(directory.path(), "edge.key", EDGE_KEY);

    let upstream = MockUpstream::start(Reply::Body {
        body: b"ok".to_vec(),
        chunk: usize::MAX,
    })
    .await;

    // `edge` authenticates every request before routing, and `g` tags it in the global scope; `r` tags
    // it in the route scope. The global list names the tagger first on purpose: the class order still
    // puts the authenticator in front, so a refused request never reaches `g`.
    let document = Resources::new()
        .upstream("api-backend", upstream.address())
        .plugin(Plugin::api_key_in("edge", "x-edge-key", &key_ref))
        .plugin(Plugin::mock_tag("g", "x-order", "global"))
        .plugin(Plugin::mock_tag("r", "x-order", "route"))
        .global_plugins(&["g", "edge"])
        .route(Route::new("api", "/api/", "api-backend").plugins(&["r"]))
        .render();

    // The mock kind exists only here: the production registry is what a deployment gets, and this
    // one adds a recorder to it.
    let recorder = MockRecorder::new();
    let mut registry = nico_plugins::builtin_registry();
    registry
        .register(MockFactory::new(Arc::clone(&recorder)))
        .expect("the mock kind is registered once");

    let source = TestSource::with_document(&document);
    let server = RunningServer::start_with_registry(directory.path(), source, registry).await;
    let address = server.address();

    // 1. A path that matches no route. Without the global chain this could only be a 404; a 401 means
    //    the policy ran before routing decided there was nothing to match.
    let anonymous = Request::get("/missing", "global.test");
    let refused = wait_for_status(address, &anonymous, 401).await;
    assert_eq!(
        refused.header("x-nico-error-code"),
        Some("auth_missing"),
        "the global plugin refuses the request it ran on"
    );
    assert_eq!(
        recorder.events(),
        Vec::new(),
        "the refusal stopped the global chain, so the tagger after it never ran"
    );

    // 2. The same path, with the credential. The global chain now continues into `g` and only then is
    //    the path found to match no route — which is what makes the 404 a statement about routing
    //    rather than about the policy.
    let allowed = anonymous.clone().header("x-edge-key", EDGE_KEY);
    let not_found = exchange(address, &allowed).await;
    assert_eq!(
        not_found.status(),
        404,
        "a global plugin ran and the path still does not exist, got {}: {}",
        not_found.status(),
        not_found.text()
    );
    let recorded = recorder.wait_for(BUDGET, |events| !events.is_empty()).await;
    let plugins: Vec<&str> = recorded.iter().map(Event::plugin).collect();
    assert_eq!(
        plugins,
        ["g"],
        "the global chain reached its second entry before routing answered 404"
    );
    assert_eq!(
        recorded.len(),
        1,
        "one request reached the tagger, and it was the routed-miss one"
    );

    // 3. The routed request: the route scope runs after the global scope, so the header carries the
    //    global value first and the route value appended to it.
    let routed = Request::get("/api/items", "global.test").header("x-edge-key", EDGE_KEY);
    let response = exchange(address, &routed).await;
    assert_eq!(
        response.status(),
        200,
        "the authenticated routed request is forwarded, got {}: {}",
        response.status(),
        response.text()
    );
    let observed = upstream.wait_for_request(BUDGET).await;
    assert_eq!(
        observed.header("x-order"),
        Some("global,route"),
        "the global scope appended first and the route scope second"
    );
    let recorded = recorder.events();
    let plugins: Vec<&str> = recorded.iter().map(Event::plugin).collect();
    assert_eq!(
        plugins,
        ["g", "g", "r"],
        "the second request ran the global tagger and then the route one"
    );

    let exit = server.cancel_and_join().await;
    assert_eq!(exit, std::process::ExitCode::SUCCESS);
}
