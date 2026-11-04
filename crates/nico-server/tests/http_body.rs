//! Request and response bytes through a real proxy: no loss, no reordering, no buffering.
//!
//! Three claims are checked here, and each of them fails differently:
//!
//! - **Nothing is lost or reordered.** An 8 MiB body (the largest the schema allows) is uploaded and
//!   echoed back; both directions must hash to the same digest as the bytes the test wrote.
//! - **Framing does not decide the bytes.** The same payload is uploaded in 1-byte, 3-byte and
//!   pseudo-random pieces, and answered with responses written in 1-byte, 3-byte and pseudo-random
//!   pieces. Pingora's chunk boundaries are its own, so only the concatenation is asserted.
//! - **Routing never reads the body.** The upstream must receive the request head while the client
//!   is still holding its first body byte. A gateway that waited for the body (or buffered it)
//!   could not get there, and the client would wait for ever — so the wait is bounded.

mod support;

use std::{net::SocketAddr, time::Duration};

use sha2::{Digest, Sha256};
use support::http::{
    Resources, Route, RunningServer,
    client::{Client, READ_BUDGET, Request},
    free_port,
    source::TestSource,
    upstream::{MockUpstream, Reply},
    write_bootstrap,
};

/// The largest body the document schema accepts (8 MiB).
const EIGHT_MIB: usize = 8 * 1024 * 1024;

/// The body used for framing variants: large enough to cross every internal buffer, small enough to
/// write one byte at a time without turning the test into a benchmark.
const FRAMED_BODY: usize = 8 * 1024;

/// How long a fixture may take to reach a state it was told to reach.
const BUDGET: Duration = Duration::from_secs(20);

#[tokio::test(flavor = "current_thread")]
async fn every_byte_survives_the_proxy_and_routing_happens_before_the_body() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();
    write_bootstrap(directory.path(), port, false, 1, 1000);

    let echo = MockUpstream::start(Reply::Echo).await;
    let spread = MockUpstream::start(Reply::Body {
        body: payload(FRAMED_BODY, 1),
        chunk: 1,
    })
    .await;
    let triple = MockUpstream::start(Reply::Body {
        body: payload(FRAMED_BODY, 2),
        chunk: 3,
    })
    .await;
    let mixed = MockUpstream::start(Reply::Body {
        body: payload(FRAMED_BODY, 3),
        chunk: 997,
    })
    .await;
    // A route of its own, so "the head arrived first" cannot be confused with an earlier request.
    let head_first = MockUpstream::start(Reply::Echo).await;

    let document = Resources::new()
        .body_limit(EIGHT_MIB as u32)
        .upstream("echo", echo.address())
        .upstream("spread", spread.address())
        .upstream("triple", triple.address())
        .upstream("mixed", mixed.address())
        .upstream("head-first", head_first.address())
        .route(Route::new("echo", "/echo", "echo"))
        .route(Route::new("spread", "/spread", "spread"))
        .route(Route::new("triple", "/triple", "triple"))
        .route(Route::new("mixed", "/mixed", "mixed"))
        .route(Route::new("head-first", "/head-first", "head-first"))
        .render();
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source).await;
    let address = server.address();

    // --- 8 MiB in both directions ------------------------------------------------------------
    let large = payload(EIGHT_MIB, 4);
    let echo_request = Request::post("/echo", "body.test").body(large.clone(), usize::MAX);
    let response = send(address, &echo_request).await;
    assert_eq!(
        response.len(),
        large.len(),
        "the echo must return as many bytes as it received (got {} of {})",
        response.len(),
        large.len()
    );
    assert_eq!(
        sha256(&response),
        sha256(&large),
        "the 8 MiB body changed while passing through the proxy"
    );

    // --- framing does not decide the bytes ---------------------------------------------------
    let small = payload(FRAMED_BODY, 5);
    for sizes in [vec![1usize], vec![3], pseudo_random_sizes(64)] {
        let echoed = upload_in_pieces(address, "/echo", &small, &sizes).await;
        assert_eq!(
            echoed,
            small,
            "a request written in pieces of {sizes:?} came back changed ({} of {} bytes)",
            echoed.len(),
            small.len()
        );
    }
    for (path, chunk) in [("/spread", 1usize), ("/triple", 3), ("/mixed", 997)] {
        let response = send(address, &Request::get(path, "body.test")).await;
        assert_eq!(
            response.len(),
            FRAMED_BODY,
            "the upstream wrote `{path}` in {chunk}-byte pieces; the proxy returned {} bytes",
            response.len()
        );
        assert_eq!(
            sha256(&response),
            sha256(&payload(
                FRAMED_BODY,
                if chunk == 1 {
                    1
                } else if chunk == 3 {
                    2
                } else {
                    3
                }
            )),
            "a response written in {chunk}-byte pieces came back changed"
        );
    }

    // --- routing happened before the body ----------------------------------------------------
    let first = small
        .get(..64)
        .expect("the fixture payload is longer than one piece")
        .to_vec();
    let mut client = Client::connect(address).await;
    let request = Request::post("/head-first", "body.test")
        // A hop-by-hop header the upstream must never see.
        .header("connection", "close")
        .body(small.clone(), usize::MAX);
    client
        .write_head(&request)
        .await
        .expect("the request head is written");

    // Wait for the upstream to report the head. Nothing of the body has been written yet, so this
    // can only return if the proxy forwarded the head without waiting for the body.
    let observed = head_first.wait_for_request(BUDGET).await;
    let body_started = std::time::Instant::now();
    assert!(
        observed.head_at() < body_started,
        "the upstream received the request head at {:?}, after the client started the body at {:?}",
        observed.head_at(),
        body_started
    );
    assert_eq!(
        head_first.observation().connections,
        1,
        "the upstream connection must exist before the first body byte"
    );
    let declared = small.len().to_string();
    assert_eq!(
        observed.header("content-length"),
        Some(declared.as_str()),
        "the upstream must have received the declared length with the head"
    );
    // The forwarding contract: `Host` is rewritten to the upstream, the client's authority moves to
    // `X-Forwarded-Host`, the scheme is stated once, and the client's connection is left behind.
    assert_eq!(
        observed.header("host"),
        Some(format!("127.0.0.1:{}", head_first.address().port()).as_str()),
        "the upstream must be addressed by its own authority, not by the client's Host"
    );
    assert_eq!(observed.header("x-forwarded-host"), Some("body.test"));
    assert_eq!(observed.header("x-forwarded-proto"), Some("http"));
    assert!(
        observed
            .header("x-forwarded-for")
            .is_some_and(|value| value.starts_with("127.0.0.1")),
        "the client address must be appended to `X-Forwarded-For`, got {:?}",
        observed.header("x-forwarded-for")
    );
    assert_eq!(
        observed.header("connection"),
        None,
        "a hop-by-hop header must not be forwarded to the upstream"
    );

    client
        .write_bytes(&first)
        .await
        .expect("the first body piece is written");
    for piece in small[first.len()..].chunks(1024) {
        client
            .write_bytes(piece)
            .await
            .expect("a body piece is written");
    }
    let head = client.read_head(READ_BUDGET).await;
    assert_eq!(head.status(), 200, "the delayed upload must still succeed");
    let echoed = client.read_body(&head, READ_BUDGET).await;
    assert_eq!(echoed, small, "the delayed upload must be echoed unchanged");

    let observed = head_first.wait_for_request(BUDGET).await;
    assert_eq!(observed.body_len(), small.len());
    assert!(
        observed
            .first_body_at()
            .is_some_and(|first_at| first_at >= body_started),
        "the upstream must have received the head before the first body chunk"
    );

    assert_eq!(
        server.cancel_and_join().await,
        std::process::ExitCode::SUCCESS
    );
}

/// Sends `request` on a fresh connection and returns the response body.
// A helper that cannot drive its fixture has nothing useful to return.
#[expect(
    clippy::expect_used,
    reason = "a helper that cannot write its fixture cannot continue"
)]
async fn send(address: SocketAddr, request: &Request) -> Vec<u8> {
    let mut client = Client::connect(address).await;
    client
        .write_request(request)
        .await
        .expect("the request is written");
    let head = client.read_head(READ_BUDGET).await;
    let body = client.read_body(&head, READ_BUDGET).await;
    assert_eq!(
        head.status(),
        200,
        "{} {} answered {} with {} body bytes",
        request.method(),
        request.path(),
        head.status(),
        body.len()
    );
    body
}

/// Uploads `payload` in pieces whose sizes cycle through `sizes`, then returns the echoed body.
#[expect(
    clippy::expect_used,
    reason = "a helper that cannot write its fixture cannot continue"
)]
async fn upload_in_pieces(
    address: SocketAddr,
    path: &str,
    payload: &[u8],
    sizes: &[usize],
) -> Vec<u8> {
    let mut client = Client::connect(address).await;
    let request = Request::post(path, "body.test").body(payload.to_vec(), 1);
    client
        .write_head(&request)
        .await
        .expect("the request head is written");
    let mut offset = 0;
    let mut index = 0;
    while offset < payload.len() {
        let size = sizes[index % sizes.len()].min(payload.len() - offset);
        client
            .write_bytes(&payload[offset..offset + size])
            .await
            .expect("a body piece is written");
        offset += size;
        index += 1;
    }
    let head = client.read_head(READ_BUDGET).await;
    assert_eq!(head.status(), 200);
    client.read_body(&head, READ_BUDGET).await
}

/// Builds a payload of `length` bytes that repeats differently for each `tag`.
fn payload(length: usize, tag: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(length);
    let mut state = 0x9e37_79b9_7f4a_7c15u64 ^ (tag as u64);
    while bytes.len() < length {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        bytes.extend_from_slice(&state.to_be_bytes());
    }
    bytes.truncate(length);
    bytes
}

/// Returns pseudo-random piece sizes in `1..=4096`, deterministic for a given call order.
fn pseudo_random_sizes(count: usize) -> Vec<usize> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    (0..count)
        .map(|_index| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as usize % 4096 + 1
        })
        .collect()
}

/// Returns the lowercase hex SHA-256 of `bytes`.
fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
