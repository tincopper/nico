//! Fixtures shared by this crate's unit tests.
//!
//! `Route`, `Upstream` and `RequestBounds` have private fields and no public constructor, so a fixture is a
//! YAML document parsed by [`Document::parse`] — the same path production configuration takes,
//! which keeps a fixture from asserting against a state that could never be published.

use std::{
    net::{IpAddr, Ipv4Addr},
    sync::Arc,
    time::Instant,
};

use nico_core::{
    ConfigDigest, Document, Generation, ParseLimits, Revision, Route, RuntimeSnapshot,
};
use pingora::http::{RequestHeader, ResponseHeader};

use crate::{
    context::HttpContext,
    plugin::context::{ContextState, Phase},
};

/// A complete upstream entry, spliced into every fixture that only cares about routes.
pub(crate) const API_BACKEND: &str = concat!(
    "  - name: api-backend\n",
    "    scheme: http\n",
    "    host: 127.0.0.1\n",
    "    port: 8080\n",
);

/// Builds a document whose only upstream is [`API_BACKEND`].
pub(crate) fn document(routes: &str) -> Document {
    document_with(API_BACKEND, routes)
}

/// Builds a document from the bodies of the `upstreams` and `routes` collections.
///
/// The bodies are spliced verbatim, so a caller controls indentation and only states the entries it
/// asserts about.
pub(crate) fn document_with(upstreams: &str, routes: &str) -> Document {
    let yaml = format!(
        concat!(
            "apiVersion: gateway/v1\n",
            "request:\n",
            "  max_body_bytes: 1048576\n",
            "  timeout_ms: 30000\n",
            "plugins: []\n",
            "providers: []\n",
            "upstreams:\n",
            "{}\n",
            "routes:\n",
            "{}\n",
        ),
        upstreams, routes
    );
    Document::parse(yaml.as_bytes(), ParseLimits::new(64 * 1024)).expect("fixture is valid")
}

/// Returns the route with `id`, panicking when the fixture no longer defines it.
pub(crate) fn route<'a>(document: &'a Document, id: &str) -> &'a Route {
    document
        .routes()
        .iter()
        .find(|route| route.id() == id)
        .expect("fixture defines the route")
}

/// Builds a request with no `Host` header, so the authority form can be exercised on its own.
pub(crate) fn request(method: &str, path: &str) -> RequestHeader {
    RequestHeader::build(method, path.as_bytes(), None).expect("request builds")
}

/// Builds an origin-form request addressed to `host`, as an HTTP/1 client would.
pub(crate) fn request_to(host: &str, path: &str) -> RequestHeader {
    let mut request = request("GET", path);
    request.insert_header("host", host).expect("host header");
    request
}

/// Builds a one-generation snapshot holding [`document`]'s upstream and `routes`.
pub(crate) fn snapshot(routes: &str) -> Arc<RuntimeSnapshot> {
    let document = document(routes);
    Arc::new(RuntimeSnapshot::new(
        Generation::new(1),
        Revision::comparable(1),
        ConfigDigest::of(routes.as_bytes()),
        document,
        "test",
    ))
}

/// Builds a one-generation snapshot of a document that declares `plugins` and `routes`.
///
/// A route that *names* a plugin is what makes the published-chain lookup observable: the zero-cost
/// path and "the plan is incomplete" path differ only by what the route declared, and the document
/// model refuses a name the collection does not declare.
pub(crate) fn snapshot_with_plugins(plugins: &str, routes: &str) -> Arc<RuntimeSnapshot> {
    let yaml = format!(
        concat!(
            "apiVersion: gateway/v1\n",
            "request:\n",
            "  max_body_bytes: 1048576\n",
            "  timeout_ms: 30000\n",
            "providers: []\n",
            "upstreams:\n",
            "{}\n",
            "routes:\n",
            "{}\n",
            "plugins:\n",
            "{}\n",
        ),
        API_BACKEND, routes, plugins
    );
    let document =
        Document::parse(yaml.as_bytes(), ParseLimits::new(64 * 1024)).expect("fixture is valid");
    Arc::new(RuntimeSnapshot::new(
        Generation::new(1),
        Revision::comparable(1),
        ConfigDigest::of(yaml.as_bytes()),
        document,
        "test",
    ))
}

/// The route every [`PluginFixture`] request matched.
///
/// The prefix is part of the fixture: it is what `route_id` and `target` answer with, and a test that
/// changes it is asserting about a different route.
pub(crate) const PLUGIN_ROUTE: &str = concat!(
    "  - id: api\n",
    "    path_prefix: /api\n",
    "    upstream: api-backend\n",
);

/// One request, its per-request state and (once a response exists) its response header.
///
/// A [`ContextState`] borrows the request and the state, so a test cannot keep one around; it borrows
/// this holder for the length of one assertion instead. Keeping the three values together is what
/// makes "one plugin stage" expressible without a live server.
pub(crate) struct PluginFixture {
    pub(crate) http: HttpContext,
    pub(crate) request: RequestHeader,
    pub(crate) response: Option<ResponseHeader>,
}

impl PluginFixture {
    /// Builds a request that already matched [`PLUGIN_ROUTE`], as a route-scoped plugin sees it.
    ///
    /// The trace id and the caller address are recorded the way the data plane records them, so a
    /// test asserts about facts a policy is actually given.
    pub(crate) fn started() -> Self {
        let snapshot = snapshot(PLUGIN_ROUTE);
        let route = &snapshot.document().routes()[0];
        let request = request_to("example.test", "/api/items?page=2");
        let mut http = HttpContext::default();
        http.begin(&snapshot, &request, Instant::now());
        http.bind_route(route);
        http.set_trace_id("trace-1");
        http.set_client_ip(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7)));
        Self {
            http,
            request,
            response: None,
        }
    }

    /// Builds a request that matched nothing yet, as a global plugin sees it.
    ///
    /// The context is empty rather than begun, so a test can record the facts a global stage has —
    /// and only those: no route id, no target, no identity.
    pub(crate) fn unrouted() -> Self {
        Self {
            http: HttpContext::default(),
            request: request_to("example.test", "/api/items?page=2"),
            response: None,
        }
    }

    /// Attaches a response with `status`, as a response stage runs against.
    pub(crate) fn responded(mut self, status: u16) -> Self {
        self.response = Some(ResponseHeader::build(status, None).expect("the status is valid"));
        self
    }

    /// Returns the context of one stage, holding this fixture's request and response.
    pub(crate) fn state(&mut self, phase: Phase) -> ContextState<'_> {
        let state = ContextState::new(
            phase,
            &mut self.http,
            &mut self.request,
            Arc::from("fake"),
            Arc::from(r#"{"header":"x-api-key"}"#),
        );
        match self.response.as_mut() {
            Some(response) => state.with_response(response),
            None => state,
        }
    }
}

mod llm;

pub(crate) use llm::{
    FakeLlmRoute, FakeLlmRoutes, LlmCalls, llm_context, llm_snapshot, llm_snapshot_with,
};
