//! A plugin that answers instead of forwarding: `Outcome::Respond`, cleaned by the one writer.
//!
//! A synthesized answer is not a refusal and not a forward — a preflight, a mock, a cache hit — so its
//! headers are *not* the refusal closed set: an end-to-end header the plugin produced survives. What
//! does not survive is what the data plane owns: the framing (`content-length` is recomputed from the
//! bytes actually written), the hop-by-hop headers, and the two headers only a typed refusal may
//! carry (`www-authenticate`, `retry-after`).
//!
//! The fixture therefore hands the writer a deliberately wrong `content-length`, a `connection`, the
//! two headers only a typed refusal may carry, and a `set-cookie` it must keep. The upstream is a real
//! mock so "nothing was forwarded" is an observation rather than an assumption: its request list must
//! stay empty.

mod support;

use std::sync::Arc;

use support::http::{
    RunningServer,
    client::{Request, get},
    free_port,
    mock::{Event, MockFactory, MockRecorder},
    resources::{Plugin, Resources, Route},
    source::TestSource,
    upstream::{MockUpstream, Reply},
    wait_for_status, write_bootstrap,
};

/// The body the plugin produces, and the length the data plane must recompute.
const BODY: &str = "made";

#[tokio::test(flavor = "current_thread")]
async fn a_synthesized_response_is_cleaned_and_written_without_an_upstream() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();
    write_bootstrap(directory.path(), port, false, 1, 1000);

    let upstream = MockUpstream::start(Reply::Body {
        body: b"upstream-never-answers".to_vec(),
        chunk: usize::MAX,
    })
    .await;

    let document = Resources::new()
        .upstream("api-backend", upstream.address())
        .plugin(Plugin::mock_respond(
            "synthetic",
            201,
            &[
                // A length that describes no body this writer will send.
                ("content-length", "999"),
                // Hop-by-hop: a synthesized response never travelled over a connection. `close` also
                // nominates nothing the writer needs, so keeping it would merely end the connection.
                ("connection", "close"),
                // Refusal-only: these belong to a typed refusal, so an answer may not carry them even
                // though the writer would happily put either on the wire.
                ("www-authenticate", "Bearer realm=forged"),
                ("retry-after", "120"),
                // End-to-end: an answer is supposed to look like a real one.
                ("x-mock", "1"),
                ("set-cookie", "a=b"),
            ],
            BODY,
        ))
        .route(Route::new("synthetic", "/synthetic/", "api-backend").plugins(&["synthetic"]))
        .render();

    let recorder = MockRecorder::new();
    let mut registry = nico_plugins::builtin_registry();
    registry
        .register(MockFactory::new(Arc::clone(&recorder)))
        .expect("the mock kind is registered once");

    let source = TestSource::with_document(&document);
    let server = RunningServer::start_with_registry(directory.path(), source, registry).await;
    let address = server.address();

    // The 201 is the readiness signal as well as the subject: it can only be produced once the
    // document is compiled and its chain published.
    let request = Request::get("/synthetic/preflight", "respond.test");
    let response = wait_for_status(address, &request, 201).await;
    assert_eq!(response.body(), BODY.as_bytes());

    assert_eq!(
        response.header("content-length"),
        Some("4"),
        "the length is recomputed from the bytes written, not taken from the plugin"
    );
    assert_eq!(
        response.header("x-mock"),
        Some("1"),
        "an end-to-end header a plugin produced survives cleaning"
    );
    assert_eq!(
        response.header("set-cookie"),
        Some("a=b"),
        "a synthesized answer may set cookies, exactly like a real one"
    );
    assert_ne!(
        response.header("connection"),
        Some("close"),
        "the plugin's hop-by-hop header is dropped; whether the connection is reused is the writer's \
         decision, so another value may be present"
    );
    assert_eq!(
        response.header("x-nico-error-code"),
        None,
        "a synthesized answer is not a refusal and carries no stable error code"
    );
    assert_eq!(
        response.header("www-authenticate"),
        None,
        "a synthesized answer may not forge a challenge, even though the plugin supplied one"
    );
    assert_eq!(
        response.header("retry-after"),
        None,
        "and may not forge a wait either"
    );

    // Nothing was forwarded: the answer replaced the request rather than travelling with it. A second
    // request reads the route table and the chain, not a cache, so one refusal to forward is one
    // observation and the count stays zero.
    let _again = get(address, "/synthetic/preflight", "respond.test").await;
    assert!(
        upstream.observation().requests.is_empty(),
        "a synthesized response ends the request before a peer is chosen: {}",
        upstream.observation().summary()
    );
    assert_eq!(
        recorder.events().len(),
        2,
        "each request ran the answering stage once"
    );
    assert!(
        recorder
            .events()
            .iter()
            .all(|event| matches!(event, Event::Request { .. })),
        "an answering plugin runs in the request stage and nowhere else"
    );

    let exit = server.cancel_and_join().await;
    assert_eq!(exit, std::process::ExitCode::SUCCESS);
}
