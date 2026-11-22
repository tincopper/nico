//! The trust boundary, end to end: what the gateway keys its ip_hash pools on when the deployment
//! declares (or withholds) a trusted proxy.
//!
//! The observable is an ip_hash pool behind two origins: the pool keys its rendezvous on the derived
//! client address, so "which origin serves this caller" is a stable function of the address the
//! derivation produced. A forged `X-Forwarded-For` from a peer the deployment does not trust must
//! leave that function untouched — every request lands on one origin no matter what the header
//! alternates between.

mod support;

use std::net::SocketAddr;

use support::http::{
    RunningServer,
    client::{Request, exchange},
    resources::{Resources, Route},
    source::TestSource,
    upstream::{MockUpstream, Reply},
};

/// The canary address a (would-be) spoofer puts in the header.
const FORGED: &str = "198.51.100.66";

/// A bootstrap that declares `proxies` (or none) as the proxies it trusts, one canary IP wide.
fn bootstrap_with_proxies(directory: &std::path::Path, proxies: Option<&[&str]>) -> String {
    let trusted = proxies.map_or_else(String::new, |list| {
        format!(
            "  trusted_proxies: [{}]\n",
            list.iter()
                .map(|proxy| format!("\"{proxy}\""))
                .collect::<Vec<_>>()
                .join(", ")
        )
    });
    format!(
        "\
config_source:
  type: local
  path: {document}
  max_bytes: 1048576
  watch: notify
  debounce_ms: 0
  max_wait_ms: 0
  cache_dir: {cache}
  allow_bootstrap_cache: false
runtime:
  threads: 1
  grace_period_ms: 1
  graceful_shutdown_ms: 1000
http:
  h2c: false
{trusted}  listeners:
    - listen: 127.0.0.1:{port}
",
        document = directory.join("resources.yaml").display(),
        cache = directory.join("cache").display(),
        port = crate::support::http::free_port(),
    )
}

/// A document whose one route pools two origins with the sticky strategy.
fn pooled_document(first: SocketAddr, second: SocketAddr) -> String {
    Resources::new()
        .upstream("origin-one", first)
        .upstream("origin-two", second)
        .route(
            Route::new("api", "/api/", "origin-one")
                .upstream_pool(&[("origin-one", 1), ("origin-two", 1)], "ip_hash"),
        )
        .render()
}

#[tokio::test(flavor = "current_thread")]
async fn a_trusted_proxy_s_xff_keys_the_sticky_pool() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    std::fs::write(
        directory.path().join("bootstrap.yaml"),
        bootstrap_with_proxies(directory.path(), Some(&["127.0.0.1/32"])),
    )
    .expect("the bootstrap fixture writes");

    let one = MockUpstream::start(Reply::Body {
        body: b"one".to_vec(),
        chunk: usize::MAX,
    })
    .await;
    let two = MockUpstream::start(Reply::Body {
        body: b"two".to_vec(),
        chunk: usize::MAX,
    })
    .await;
    let document = pooled_document(one.address(), two.address());
    std::fs::write(directory.path().join("resources.yaml"), &document)
        .expect("the resources fixture writes");

    let source = TestSource::with_document(&document);
    // `RunningServer::start` loads `bootstrap.yaml` from the directory: the file this fixture wrote
    // above is the deployment under test.
    let server = RunningServer::start(directory.path(), source).await;

    // The same chain keeps meeting the same origin: the derivation produced one address for this
    // caller, and the pool keys on it.
    let request =
        || Request::get("/api/items", "a.test").header("x-forwarded-for", "198.51.100.7, 10.0.0.5");
    let mut first_origin = String::new();
    for _ in 0..8 {
        let response = exchange(server.address(), &request()).await;
        assert_eq!(response.status(), 200, "{}", response.text());
        let body = response.text();
        if first_origin.is_empty() {
            first_origin = body.clone();
        }
        assert_eq!(
            body, first_origin,
            "the same XFF caller keeps meeting the same origin"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_forged_xff_from_an_untrusted_peer_changes_nothing() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    // No `trusted_proxies`: the deployment trusts nobody, so every XFF is ignored wholesale.
    std::fs::write(
        directory.path().join("bootstrap.yaml"),
        bootstrap_with_proxies(directory.path(), None),
    )
    .expect("the bootstrap fixture writes");

    let one = MockUpstream::start(Reply::Body {
        body: b"one".to_vec(),
        chunk: usize::MAX,
    })
    .await;
    let two = MockUpstream::start(Reply::Body {
        body: b"two".to_vec(),
        chunk: usize::MAX,
    })
    .await;
    let document = pooled_document(one.address(), two.address());
    std::fs::write(directory.path().join("resources.yaml"), &document)
        .expect("the resources fixture writes");

    let source = TestSource::with_document(&document);
    // `RunningServer::start` loads `bootstrap.yaml` from the directory: the file this fixture wrote
    // above is the deployment under test.
    let server = RunningServer::start(directory.path(), source).await;

    // Alternating forged values: if the gateway honored an untrusted peer's header, the ip_hash
    // draw would flip between origins with near certainty. One origin for every request is the
    // proof that the header was never read.
    let mut first_origin = String::new();
    for round in 0..16 {
        let forged = if round % 2 == 0 {
            FORGED
        } else {
            "198.51.100.99"
        };
        let request = Request::get("/api/items", "a.test").header("x-forwarded-for", forged);
        let response = exchange(server.address(), &request).await;
        assert_eq!(response.status(), 200, "{}", response.text());
        let body = response.text();
        if first_origin.is_empty() {
            first_origin = body.clone();
        }
        assert_eq!(
            body, first_origin,
            "a forged header from an untrusted peer must not move the sticky pool"
        );
    }
}
