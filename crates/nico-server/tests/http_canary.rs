//! A traffic route's upstream pool, end to end: two real origins behind one pooled route.
//!
//! The distribution phase asserts that both origins serve traffic; the convergence phase drives one
//! origin's members into ejection and asserts the whole share lands on the other. Both phases run
//! against one gateway serving one document, which is what only a real server can prove: the pool is
//! wired from the document the request was routed with, not from whatever the store holds now.

mod support;

use support::{
    http::{
        RunningServer,
        client::{Request, get},
        free_port,
        resources::{Resources, Route},
        source::TestSource,
        upstream::{MockUpstream, Reply},
        wait_for_status, write_bootstrap,
    },
    token,
};

/// How many requests the distribution phase may make before both origins must have served one.
///
/// Equal weights make the odds `2^-N` against one side never drawing; forty is far past any
/// environment's jitter.
const SAMPLE: usize = 40;

/// The tail of consecutive successful requests the convergence phase asserts.
const STABLE_TAIL: usize = 20;

/// The cap on one phase's requests, so a broken pool fails the test instead of hanging it.
const CAP: usize = 120;

#[tokio::test(flavor = "current_thread")]
async fn a_pooled_route_serves_both_origins_and_converges_when_one_fails() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();
    let _bootstrap = write_bootstrap(directory.path(), port, false, 1, 1000);

    // The stable origin answers with its name; the canary origin answers `500`, so its ejection can
    // be driven from the client side without touching the mock's internals.
    let stable = MockUpstream::start(Reply::Body {
        body: b"stable".to_vec(),
        chunk: usize::MAX,
    })
    .await;
    let canary = MockUpstream::start(Reply::Response {
        status: 500,
        headers: Vec::new(),
        body: b"canary".to_vec(),
        chunk: usize::MAX,
    })
    .await;

    let provider_address = format!("http://127.0.0.1:{}", free_port());
    let provider_key = token::secret_file(directory.path(), "provider.key", "sk-fixture");

    // Equal weights: the distribution phase does not need to reason about proportions, only about
    // reachability. The canary upstream carries a long cooldown so that once it is ejected, no probe
    // interrupts the stable tail the convergence phase asserts.
    let document = Resources::new()
        .provider("openai", &provider_address, &provider_key)
        .upstream("stable", stable.address())
        .upstream("canary", canary.address())
        .route(
            Route::new("api", "/api/", "stable")
                .upstream_pool(&[("stable", 1), ("canary", 1)], "weighted_random"),
        )
        .render();
    // The document rides in the source, so the startup publication is the pooled generation this
    // scenario asserts against.
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source).await;
    let address = server.address();
    wait_for_status(address, &Request::get("/api/items", "a.test"), 200).await;

    // --- distribution: both origins serve, in proportions the weights leave open ----------------
    let mut saw_stable = false;
    let mut saw_canary = false;
    for _ in 0..SAMPLE {
        let response = get(address, "/api/items", "a.test").await;
        // The failing origin's 500 is the origin's own answer, forwarded as it arrived — the pool's
        // refusal is a 502, and a 502 here would mean the gateway answered for itself.
        match response.text().as_str() {
            "stable" => saw_stable = true,
            "canary" => saw_canary = true,
            other => panic!("unexpected body {other:?} (status {})", response.status()),
        }
    }
    assert!(
        saw_stable && saw_canary,
        "equal weights over {SAMPLE} draws must reach both origins (stable: {saw_stable}, canary: {saw_canary})"
    );

    // --- convergence: the failing origin is ejected, and the whole share lands on the stable one --
    //
    // The distribution phase already drove the canary's failure count past its budget, so its
    // upstream is out of the candidates by now. The tail is the proof: with the canary still
    // eligible, twenty consecutive stable answers would be a one-in-a-million draw; with the
    // ejection working, they are the only thing that can happen.
    let mut stable_tail = 0usize;
    for _ in 0..CAP {
        let response = get(address, "/api/items", "a.test").await;
        if response.status() == 200 && response.text() == "stable" {
            stable_tail += 1;
            if stable_tail >= STABLE_TAIL {
                break;
            }
            continue;
        }
        // A request the pool still routed to the failing origin: it was admitted before the ejection
        // completed, and the streak restarts because that attempt did not serve.
        stable_tail = 0;
    }
    assert!(
        stable_tail >= STABLE_TAIL,
        "after the ejection, {STABLE_TAIL} consecutive requests must land on the stable origin"
    );
}
