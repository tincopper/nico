//! An in-process etcd: the "environment" the real client is verified against.
//!
//! It serves the **official** etcd v3 gRPC protocol — the server half generated from the same
//! vendored protos the real client's wire speaks — backed by an in-memory store that keeps the full
//! revision history of one key. Tests hold its handle and drive the store directly (put, delete,
//! compact, require auth), so every scenario the roadmap asked to verify has a deterministic way to
//! happen, and the wire between the real client and this server is a real gRPC connection over
//! loopback.
//!
//! Two independent axes compose through [`MockServer::builder`]: the transport (plaintext, or TLS
//! with the certificate a test hands over as bytes) and the access rule (anonymous, or a credential
//! pair exchanged for a token). The bytes are the point — this crate never learns where a test keeps
//! its fixtures, exactly as it never learns where a deployment keeps its trust root.
//!
//! What it deliberately does **not** model: client certificates (a server may present one, it never
//! demands one), leases, transactions, multi-endpoint failover, token refresh, and anything else a
//! real cluster does that the adapter contract does not observe. The adapter folds what it cannot
//! understand into resyncs and unavailability; this server produces those situations and nothing
//! else.
//!
//! # Modules
//!
//! - `store` — the revision history and the live tail one instance serves.
//! - `rpc` — the KV and Watch services, translating the store onto the wire.
//! - `auth` — the credential pair, the token it mints, and the check in front of both services.

use std::{fmt, net::SocketAddr, sync::Arc};

use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Identity, ServerTlsConfig};

use crate::client::proto::pb::etcdserverpb::{
    auth_server::AuthServer, kv_server::KvServer, watch_server::WatchServer,
};

mod auth;
mod rpc;
mod store;

use auth::{MockAuth, RequiredAuth, interceptor};
use rpc::{MockKv, MockWatch};
pub use store::StoreState;

/// One running server: its address, the transport it serves, and the store handle its tests drive.
pub struct MockServer {
    address: SocketAddr,
    scheme: &'static str,
    state: Arc<StoreState>,
}

impl MockServer {
    /// Starts a plaintext, anonymous server on an ephemeral loopback port.
    ///
    /// # Errors
    /// Returns an error when the loopback port cannot be bound or reported.
    pub async fn start() -> std::io::Result<Self> {
        Self::builder().start().await
    }

    /// Returns the builder that adds a TLS identity and required credentials.
    pub fn builder() -> MockServerBuilder {
        MockServerBuilder::default()
    }

    /// Returns the endpoint the real client connects to, with the scheme this server serves.
    pub fn endpoint(&self) -> String {
        format!("{}://{}", self.scheme, self.address)
    }

    /// Returns the bound port, for a test that needs to address the server by another name.
    pub fn port(&self) -> u16 {
        self.address.port()
    }

    /// Returns the store handle the test drives.
    pub fn state(&self) -> Arc<StoreState> {
        Arc::clone(&self.state)
    }
}

/// How one mock server is started: what it presents, and what it demands.
///
/// The two halves are independent and both default to absent, which is the plaintext, anonymous
/// server [`MockServer::start`] provides.
#[derive(Default)]
pub struct MockServerBuilder {
    identity: Option<(Vec<u8>, Vec<u8>)>,
    credentials: Option<(String, String)>,
}

impl MockServerBuilder {
    /// Serves TLS with this certificate chain and private key, given as PEM bytes.
    ///
    /// The endpoint the server reports then uses `https://`, so a client built from it is configured
    /// for the transport this server actually speaks.
    #[must_use]
    pub fn identity(
        mut self,
        certificate_pem: impl Into<Vec<u8>>,
        key_pem: impl Into<Vec<u8>>,
    ) -> Self {
        self.identity = Some((certificate_pem.into(), key_pem.into()));
        self
    }

    /// Requires this user and password, exchanged for a token before any read is served.
    #[must_use]
    pub fn credentials(mut self, user: impl Into<String>, password: impl Into<String>) -> Self {
        self.credentials = Some((user.into(), password.into()));
        self
    }

    /// Starts the KV, Watch and Auth services on an ephemeral loopback port.
    ///
    /// The listener is handed to the server as an incoming stream so the ephemeral port is bound
    /// exactly once — no free-port race between picking the address and serving it. The TLS
    /// configuration is built before the task is spawned, so an unusable identity is this call's
    /// error rather than a server that silently never accepts a connection.
    ///
    /// # Errors
    /// Returns an error when the loopback port cannot be bound or reported, or when the certificate
    /// and key cannot be assembled into a TLS configuration.
    pub async fn start(self) -> std::io::Result<MockServer> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let state = Arc::new(StoreState::new());
        let required = self
            .credentials
            .map(|(user, password)| Arc::new(RequiredAuth::new(user, password)));

        let kv = KvServer::with_interceptor(
            MockKv {
                state: Arc::clone(&state),
            },
            interceptor(required.clone()),
        );
        let watch = WatchServer::with_interceptor(
            MockWatch {
                state: Arc::clone(&state),
            },
            interceptor(required.clone()),
        );
        let authenticate = AuthServer::new(MockAuth { required });

        let mut server = tonic::transport::Server::builder();
        let scheme = match self.identity {
            None => "http",
            Some((certificate, key)) => {
                let tls = ServerTlsConfig::new().identity(Identity::from_pem(certificate, key));
                server = server.tls_config(tls).map_err(std::io::Error::other)?;
                "https"
            }
        };

        tokio::spawn(async move {
            let _ = server
                .add_service(kv)
                .add_service(watch)
                .add_service(authenticate)
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await;
        });
        Ok(MockServer {
            address,
            scheme,
            state,
        })
    }
}

impl fmt::Debug for MockServerBuilder {
    /// Reports which halves are configured, never their bytes.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MockServerBuilder")
            .field("has_identity", &self.identity.is_some())
            .field("has_credentials", &self.credentials.is_some())
            .finish()
    }
}
