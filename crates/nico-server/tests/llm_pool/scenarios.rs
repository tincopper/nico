//! The pool scenarios' shared vocabulary: the request, the replies, and the two waits.
//!
//! The shapes here mirror `llm_fallback/scenarios.rs` — the same request document and the same
//! provider answer — so a reader comparing the two files sees one difference: how many providers the
//! route names, and what the classification does about a failure.

use std::{net::SocketAddr, time::Duration};

use crate::support::http::{
    STARTUP_BUDGET,
    client::{Request, Response, exchange},
    resources::Resources,
    source::TestSource,
    upstream::{MockUpstream, Reply},
};

/// The provider key every fixture document references.
pub(super) const KEY: &str = "sk-pool-key";

/// Returns the loopback base URL one origin names.
pub(super) fn base_url(upstream: &MockUpstream) -> String {
    format!("http://{}", upstream.address())
}

/// A Chat Completions answer naming `model`, which is how a scenario tells two origins apart.
pub(super) fn chat_response(model: &str) -> Vec<u8> {
    format!(
        r#"{{"id":"chatcmpl-{model}","object":"chat.completion","created":0,"model":"{model}","choices":[{{"index":0,"message":{{"role":"assistant","content":"from {model}"}},"finish_reason":"stop"}}]}}"#
    )
    .into_bytes()
}

/// The caller's request body: a minimal Chat Completions document.
fn chat_request() -> Vec<u8> {
    br#"{"model":"fast","messages":[{"role":"user","content":"pool-scenario"}]}"#.to_vec()
}

/// The fixture request: a complete-answer POST to `/v1/chat/completions`.
pub(super) fn chat() -> Request {
    Request::post("/v1/chat/completions", "llm.test")
        .header("content-type", "application/json")
        .body(chat_request(), usize::MAX)
}

/// Sends the fixture request to `address` and returns the answer.
pub(super) async fn ask(address: SocketAddr, request: &Request) -> Response {
    exchange(address, request).await
}

/// Asks until the route's current generation answers, and returns that answer.
///
/// A freshly published document takes effect asynchronously, and the previous generation keeps
/// serving until it does. Asking until the origin named by the *new* document has seen a connection is
/// what makes the first measured request a request against the document the test published.
pub(super) async fn ask_until_published(
    address: SocketAddr,
    _credential: &str,
    _source: &TestSource,
) -> Response {
    let deadline = std::time::Instant::now() + STARTUP_BUDGET;
    loop {
        let response = ask(address, &chat()).await;
        if response.status() != 503 {
            return response;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the route never reached service: {}",
            response.text()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A success reply carrying `model`'s completion.
pub(super) fn success(model: &str) -> Reply {
    Reply::Response {
        status: 200,
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body: chat_response(model),
        chunk: usize::MAX,
    }
}

/// A failure reply of `status` carrying a provider error document.
pub(super) fn failure(status: u16) -> Reply {
    Reply::Response {
        status,
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body: br#"{"error":{"message":"provider-internal-detail","type":"server_error"}}"#.to_vec(),
        chunk: usize::MAX,
    }
}

/// A document whose only route is a pool over `members`.
pub(super) fn pooled_document(
    members: &[(&str, u16)],
    credential: &str,
    strategy: &'static str,
    retry_budget: u8,
    bases: &[(&str, String)],
) -> String {
    pooled_document_with_fallback(members, credential, strategy, retry_budget, bases, &[])
}

/// [`pooled_document`] with a fallback chain tried after the pool is exhausted.
pub(super) fn pooled_document_with_fallback(
    members: &[(&str, u16)],
    credential: &str,
    strategy: &'static str,
    retry_budget: u8,
    bases: &[(&str, String)],
    fallback: &[&str],
) -> String {
    let mut resources = Resources::new();
    for (name, base) in bases {
        resources = resources.provider_with_breaker(name, base, credential, 0, 30_000);
    }
    let mut route =
        crate::support::http::resources::Route::provider("llm-chat", "/v1/chat/", members[0].0)
            .pool(members, strategy)
            .retry_budget(retry_budget);
    if !fallback.is_empty() {
        route = route.fallback(fallback);
    }
    resources.route(route).render()
}
