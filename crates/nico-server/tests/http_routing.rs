//! Routing through a real Pingora server: precedence, the two "no answer" paths, and targets.
//!
//! The first request of this scenario is made **before** any snapshot exists, which is why the
//! source starts out holding its answer: a pipeline with a document in hand publishes it too fast
//! for "not ready" to be observable, and a gateway that was just started has exactly that window.
//! Every later step runs against the document published in the middle of the test, so the same
//! server proves routing, target selection and reload in the order they happen in production.

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

#[tokio::test(flavor = "current_thread")]
async fn routing_ranks_host_before_prefix_and_answers_404_and_503() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();
    let bootstrap = write_bootstrap(directory.path(), port, false, 1, 1000);

    let broad = MockUpstream::start(Reply::Body {
        body: b"broad".to_vec(),
        chunk: usize::MAX,
    })
    .await;
    let narrow = MockUpstream::start(Reply::Body {
        body: b"narrow".to_vec(),
        chunk: usize::MAX,
    })
    .await;
    let hosted = MockUpstream::start(Reply::Body {
        body: b"hosted".to_vec(),
        chunk: usize::MAX,
    })
    .await;
    // Nothing listens on this port, so the route that names it can only fail to connect.
    let closed = free_port();
    // A published provider route addresses the endpoint its adapter compiled at publication, so the
    // fixture needs a readable credential and an address of its own. Nothing listens on this port
    // either: the scenario is about routing to the front-end, not about a provider's answer.
    let provider_address = format!("http://127.0.0.1:{}", free_port());
    let provider_key = token::secret_file(directory.path(), "provider.key", "sk-fixture");

    let source = TestSource::holding();
    let server = RunningServer::start(directory.path(), source.clone()).await;
    let address = server.address();

    // A gateway that has not published a snapshot cannot route: it answers 503, not 404 and not a
    // connection error.
    let request = Request::get("/api/v1/items", "a.test").header("x-request-id", "before-publish");
    let response = support::http::client::exchange(address, &request).await;
    assert_eq!(
        response.status(),
        503,
        "a request before the first publish must be 503, not {}: {}",
        response.status(),
        response.text()
    );
    assert_eq!(response.json_code().as_deref(), Some("not_ready"));
    assert_eq!(
        response.json_trace_id().as_deref(),
        Some("before-publish"),
        "the error body must echo the caller's correlation id so its logs can be joined"
    );

    let document = Resources::new()
        .provider("openai", &provider_address, &provider_key)
        .upstream("broad", broad.address())
        .upstream("narrow", narrow.address())
        .upstream("hosted", hosted.address())
        .upstream(
            "nowhere",
            format!("127.0.0.1:{closed}")
                .parse()
                .expect("a loopback address"),
        )
        .route(Route::new("api", "/api/", "broad"))
        .route(Route::new("api-v1", "/api/v1/", "narrow"))
        .route(Route::new("api-on-a", "/api/", "hosted").host("a.test"))
        .provider_route("to-provider", "/provider/", "openai")
        .route(Route::new("to-nowhere", "/nowhere/", "nowhere"))
        .render();
    source.publish(&document);

    // The longest prefix wins when both candidates are host-agnostic.
    let response = wait_for_status(address, &Request::get("/api/v1/items", "b.test"), 200).await;
    assert_eq!(
        response.text(),
        "narrow",
        "the longer prefix `/api/v1/` must beat `/api/`"
    );

    let response = get(address, "/api/items", "b.test").await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.text(), "broad");

    // A host-specific route outranks a host-agnostic one with the same prefix …
    let response = get(address, "/api/items", "a.test").await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.text(),
        "hosted",
        "`/api/` on `a.test` must beat the host-agnostic `/api/`"
    );

    // … and it outranks a *longer* host-agnostic prefix too, because host rank is compared first.
    let response = get(address, "/api/v1/items", "a.test").await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.text(),
        "hosted",
        "a host-specific `/api/` must beat the longer host-agnostic `/api/v1/`"
    );

    // The `Host` is compared case-insensitively and with the port removed.
    let response = get(address, "/api/items", "A.TEST:8443").await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.text(), "hosted");

    // No route at all is a 404 with the stable code, not a 502 from an empty peer.
    let request = Request::get("/nope", "a.test").header("x-request-id", "no-route");
    let response = support::http::client::exchange(address, &request).await;
    assert_eq!(
        response.status(),
        404,
        "an unmatched path must be 404, not {}: {}",
        response.status(),
        response.text()
    );
    assert_eq!(response.json_code().as_deref(), Some("route_not_found"));
    assert_eq!(response.json_trace_id().as_deref(), Some("no-route"));

    // A provider target reaches the front-end that was published for it, and the request goes to the
    // endpoint the adapter compiled with the credential the provider entry materialized. Nothing
    // listens there, so the answer is the connection failure — not `provider_unavailable`, which would
    // mean the route reached no front-end, and not a route further down the table. Whether the body was
    // translated is the translation scenarios' subject, not this one's.
    let chat = br#"{"model":"chat-small","messages":[{"role":"user","content":"hi"}]}"#;
    let request = Request::post("/provider/chat", "a.test").body(chat.to_vec(), usize::MAX);
    let response = support::http::client::exchange(address, &request).await;
    assert_eq!(
        response.status(),
        502,
        "a provider route forwards to the endpoint it published; the fixture's peer is closed, so \
         the forwarded request fails to connect, not {}: {}",
        response.status(),
        response.text()
    );
    assert_eq!(
        response.json_code().as_deref(),
        Some("upstream_unreachable"),
        "the answer is the connection failure, not `provider_unavailable`"
    );

    // An upstream that refuses the connection is a bad gateway, not a server error.
    let response = get(address, "/nowhere/items", "a.test").await;
    assert_eq!(
        response.status(),
        502,
        "an unreachable upstream must be 502, not {}: {}",
        response.status(),
        response.text()
    );
    assert_eq!(
        response.json_code().as_deref(),
        Some("upstream_unreachable")
    );

    // Only the two routed requests reached an upstream; a rejected one never does.
    assert_eq!(
        broad.observation().connections,
        1,
        "the host-agnostic `/api/` upstream must have answered exactly one request"
    );
    assert_eq!(narrow.observation().connections, 1);
    assert_eq!(hosted.observation().connections, 3);

    // The configured drain window came from the fixture, not from Pingora's 300 s default.
    assert_eq!(
        bootstrap.http().grace_period(),
        std::time::Duration::from_millis(1)
    );

    let code = server.cancel_and_join().await;
    assert_eq!(code, std::process::ExitCode::SUCCESS);
}
