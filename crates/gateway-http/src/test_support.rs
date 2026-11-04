//! Fixtures shared by this crate's unit tests.
//!
//! `Route`, `Upstream` and `Limits` have private fields and no public constructor, so a fixture is a
//! YAML document parsed by [`Document::parse`] — the same path production configuration takes,
//! which keeps a fixture from asserting against a state that could never be published.

use std::sync::Arc;

use gateway_core::{
    ConfigDigest, Document, Generation, ParseLimits, Revision, Route, RuntimeSnapshot,
};
use pingora::http::RequestHeader;

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
            "limits:\n",
            "  max_request_body_bytes: 1048576\n",
            "  request_timeout_ms: 30000\n",
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
