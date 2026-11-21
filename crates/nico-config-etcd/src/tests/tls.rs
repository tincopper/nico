//! TLS and credentials over the real wire: what a production etcd asks for, and what a client that
//! cannot answer gets.
//!
//! The certificates are the repository's own fixtures, read as bytes exactly as a deployment reads
//! its files — this crate never learns a path, which is why the mock takes PEM and the client's
//! options take PEM. A handshake that verifies, a handshake that refuses, and a token exchange that
//! refuses are the three facts these tests exist to prove; the adapter's semantics above the seam are
//! table-tested elsewhere.

use bytes::Bytes;

use crate::{
    EtcdConnectOptions, EtcdCredentials, EtcdTlsOptions, GrpcEtcdClient,
    client::{EtcdClient, EtcdError, WatchEvent, WatchOutcome, mock_server::MockServer},
    mock::MOCK_KEY,
};

/// The workspace's shared TLS fixtures, above any one crate — see `fixtures/tls/README.md`.
///
/// The leaf certificate a mock server presents on loopback: `DNS:localhost, IP:127.0.0.1`.
const LOCALHOST_PEM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/tls/localhost.pem"
));
/// The private key of [`LOCALHOST_PEM`].
const LOCALHOST_KEY: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/tls/localhost.key"
));
/// The trust root [`LOCALHOST_PEM`] chains to.
const TEST_ROOT_PEM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/tls/test-root.pem"
));
/// A root unrelated to [`LOCALHOST_PEM`]: a client configured with it trusts nothing it connects to.
const OTHER_ROOT_PEM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/tls/other-root.pem"
));
/// A leaf that also chains to [`TEST_ROOT_PEM`] but names `example.invalid` and nothing else.
const WRONG_NAME_PEM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/tls/wrong-name.pem"
));
/// The private key of [`WRONG_NAME_PEM`].
const WRONG_NAME_KEY: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/tls/wrong-name.key"
));

/// The trust material every TLS scenario uses: the test root, verified against `localhost`.
fn tls() -> EtcdTlsOptions {
    EtcdTlsOptions::new(TEST_ROOT_PEM).with_domain_name("localhost")
}

/// Starts a TLS server presenting the loopback leaf.
async fn tls_server() -> MockServer {
    MockServer::builder()
        .identity(LOCALHOST_PEM, LOCALHOST_KEY)
        .start()
        .await
        .expect("the mock etcd starts")
}

#[tokio::test]
async fn a_tls_connection_reads_and_watches() {
    let server = tls_server().await;
    server.state().put(b"doc-1");
    server.state().put(b"doc-2");
    let client = GrpcEtcdClient::connect(
        &[server.endpoint()],
        EtcdConnectOptions::new().with_tls(tls()),
    )
    .await
    .expect("the client verifies the server against the configured root");

    let entry = client
        .get(MOCK_KEY)
        .await
        .expect("the read succeeds over TLS")
        .expect("the key exists");
    assert_eq!(entry.value.as_ref(), b"doc-2");
    assert_eq!(entry.mod_revision, 2);

    let mut stream = client
        .watch(MOCK_KEY, 2)
        .await
        .expect("the watch establishes over TLS");
    let outcome = stream.recv().await.expect("an outcome").expect("delivered");
    assert_eq!(
        outcome_as_events(outcome),
        [WatchEvent::Put {
            value: Bytes::from_static(b"doc-2"),
            mod_revision: 2,
        }],
        "the watch replays from the requested revision, inclusive"
    );
}

#[tokio::test]
async fn the_configured_name_is_the_one_verified() {
    // The server presents a chain whose only name is `example.invalid`, and the endpoint addresses
    // it as `127.0.0.1`. Only a client that verifies the configured name — not the endpoint's host —
    // can complete this handshake, which is the field's whole purpose.
    let server = MockServer::builder()
        .identity(WRONG_NAME_PEM, WRONG_NAME_KEY)
        .start()
        .await
        .expect("the mock etcd starts");
    server.state().put(b"doc");

    let verified = GrpcEtcdClient::connect(
        &[server.endpoint()],
        EtcdConnectOptions::new()
            .with_tls(EtcdTlsOptions::new(TEST_ROOT_PEM).with_domain_name("example.invalid")),
    )
    .await
    .expect("the client trusts the chain and verifies the configured name");
    assert!(
        verified
            .get(MOCK_KEY)
            .await
            .expect("the read succeeds")
            .is_some(),
        "the key exists: the handshake is what the name was about"
    );

    let refused = GrpcEtcdClient::connect(
        &[server.endpoint()],
        EtcdConnectOptions::new().with_tls(EtcdTlsOptions::new(TEST_ROOT_PEM)),
    )
    .await
    .expect("a channel is built without a handshake");
    let error = refused
        .get(MOCK_KEY)
        .await
        .expect_err("the endpoint's address is not a name the chain carries");
    assert!(
        matches!(error, EtcdError::Connect { .. }),
        "a name mismatch is a transport failure, got {error:?}"
    );
}

#[tokio::test]
async fn an_untrusted_root_refuses_the_handshake() {
    let server = tls_server().await;
    server.state().put(b"doc");

    let client = GrpcEtcdClient::connect(
        &[server.endpoint()],
        EtcdConnectOptions::new()
            .with_tls(EtcdTlsOptions::new(OTHER_ROOT_PEM).with_domain_name("localhost")),
    )
    .await
    .expect("a channel is built without a handshake");
    let error = client
        .get(MOCK_KEY)
        .await
        .expect_err("the presented chain does not go back to the configured root");

    assert!(
        matches!(error, EtcdError::Connect { .. }),
        "an unverifiable server is a transport failure, got {error:?}"
    );
}

#[tokio::test]
async fn a_plaintext_client_cannot_reach_a_tls_server() {
    let server = tls_server().await;
    server.state().put(b"doc");

    let client = GrpcEtcdClient::connect(&[server.endpoint()], EtcdConnectOptions::new())
        .await
        .expect("a channel is built without a handshake");
    let error = client
        .get(MOCK_KEY)
        .await
        .expect_err("the server never speaks cleartext, so nothing downgrades");

    assert!(
        matches!(error, EtcdError::Connect { .. }),
        "expected a transport failure, got {error:?}"
    );
}

#[tokio::test]
async fn credentials_are_exchanged_for_a_token_before_a_read() {
    let server = MockServer::builder()
        .identity(LOCALHOST_PEM, LOCALHOST_KEY)
        .credentials("nico", "s3cret")
        .start()
        .await
        .expect("the mock etcd starts");
    server.state().put(b"doc");

    let client = GrpcEtcdClient::connect(
        &[server.endpoint()],
        EtcdConnectOptions::new()
            .with_tls(tls())
            .with_credentials(EtcdCredentials::new("nico", "s3cret")),
    )
    .await
    .expect("the credentials are accepted while connecting");

    let entry = client
        .get(MOCK_KEY)
        .await
        .expect("the token the exchange minted is accepted")
        .expect("the key exists");
    assert_eq!(entry.value.as_ref(), b"doc");
}

#[tokio::test]
async fn a_wrong_password_is_refused_while_connecting() {
    let server = MockServer::builder()
        .identity(LOCALHOST_PEM, LOCALHOST_KEY)
        .credentials("nico", "s3cret")
        .start()
        .await
        .expect("the mock etcd starts");

    // `etcd-client` authenticates as part of connecting, so a refused password never reaches a read.
    let error = GrpcEtcdClient::connect(
        &[server.endpoint()],
        EtcdConnectOptions::new()
            .with_tls(tls())
            .with_credentials(EtcdCredentials::new("nico", "wrong")),
    )
    .await
    .expect_err("the server refuses the exchange");

    assert!(
        matches!(error, EtcdError::Auth { .. }),
        "a refused credential is auth, not transport, got {error:?}"
    );
}

#[tokio::test]
async fn a_client_without_credentials_is_refused_by_the_store() {
    let server = MockServer::builder()
        .identity(LOCALHOST_PEM, LOCALHOST_KEY)
        .credentials("nico", "s3cret")
        .start()
        .await
        .expect("the mock etcd starts");
    server.state().put(b"doc");

    let client = GrpcEtcdClient::connect(
        &[server.endpoint()],
        EtcdConnectOptions::new().with_tls(tls()),
    )
    .await
    .expect("a client may connect anonymously");

    let error = client
        .get(MOCK_KEY)
        .await
        .expect_err("the token check refuses a request that presents none");

    assert!(
        matches!(error, EtcdError::Auth { .. }),
        "expected auth, got {error:?}"
    );
}

/// Reads the events out of one delivered outcome.
///
/// Panics when the outcome is not an events batch, which is the only outcome this client produces
/// over a wire (the create confirmation is consumed inside `etcd-client`).
fn outcome_as_events(outcome: WatchOutcome) -> Vec<WatchEvent> {
    match outcome {
        WatchOutcome::Events { events } => events,
        other => panic!("expected events, got {other:?}"),
    }
}
