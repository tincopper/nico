//! A TLS origin: a loopback listener that presents one certificate and answers one response.
//!
//! The HTTP mock in [`super::http::upstream`] speaks plaintext and exists to make an origin
//! misbehave on purpose. This one exists for the *handshake*, so it does the least an origin can do:
//! present a chosen certificate, record whether the client completed a handshake or aborted it, and
//! answer one `200` with a fixed body. The two modules share no code because they answer different
//! questions.
//!
//! A handshake that the client aborts shows up here as an `accept` failure, which is what lets a
//! security scenario state "the refusal happened during verification" instead of guessing it from a
//! status code: a `502` could also mean the peer was never reachable.
//!
//! The certificate files are static fixtures (`tests/fixtures/tls/`, described in their README), so
//! no scenario depends on a certificate-synthesis library, on the wall clock or on the host's trust
//! store.

use std::{
    fs::File,
    io::{self, BufReader},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::watch,
    task::JoinHandle,
    time::timeout,
};
use tokio_rustls::{TlsAcceptor, rustls::ServerConfig};

/// Returns the path of one file in the TLS fixture directory.
pub(crate) fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/tls")
        .join(name)
}

/// One request the origin accepted and answered.
#[derive(Debug, Clone)]
pub(crate) struct Exchange {
    /// The request head exactly as it arrived, request line and headers included.
    pub(crate) head: Vec<u8>,
    /// The request body, read according to the head's `content-length`.
    pub(crate) body: Vec<u8>,
}

impl Exchange {
    /// Returns the head as text, so a broken assertion shows what arrived instead of a byte list.
    pub(crate) fn head_text(&self) -> String {
        String::from_utf8_lossy(&self.head).into_owned()
    }

    /// Returns the request line, which is what the peer said it wanted and where.
    pub(crate) fn request_line(&self) -> &str {
        let end = self
            .head
            .windows(2)
            .position(|window| window == b"\r\n")
            .unwrap_or(self.head.len());
        std::str::from_utf8(&self.head[..end]).unwrap_or_default()
    }

    /// Returns the first value of request header `name`, case-insensitively.
    pub(crate) fn header(&self, name: &str) -> Option<String> {
        header_value(&self.head, name)
    }
}

/// What the origin observed across all its connections.
#[derive(Debug, Clone, Default)]
pub(crate) struct Observation {
    /// How many handshakes a client completed.
    pub(crate) handshakes: usize,
    /// How many handshakes a client aborted, which is how a refused certificate looks here.
    pub(crate) handshake_failures: usize,
    /// One entry per answered request, in arrival order.
    pub(crate) requests: Vec<Exchange>,
}

/// A TLS origin bound to one loopback port, presenting one certificate.
pub(crate) struct TlsOrigin {
    port: u16,
    shared: Arc<Shared>,
    /// One accept loop per bound loopback family; see [`TlsOrigin::start`].
    accept: Vec<JoinHandle<()>>,
}

impl TlsOrigin {
    /// Binds a port and starts answering with `body` over the certificate in `certificate`/`key`.
    ///
    /// The two arguments are fixture file names, not paths: a scenario chooses *which* identity to
    /// present, and the directory is the fixture's own business.
    pub(crate) async fn start(certificate: &str, key: &str, body: &[u8]) -> Self {
        let acceptor = TlsAcceptor::from(Arc::new(server_config(certificate, key)));
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the TLS origin binds a loopback port");
        let port = listener
            .local_addr()
            .expect("the origin has an address")
            .port();
        // `localhost` is a name, and a scenario that names it in a route has to reach whichever
        // loopback family this host resolves it to first — `::1` on macOS, often `127.0.0.1`
        // elsewhere. Binding the same port on IPv6 loopback as well makes the fixture independent of
        // that order; a host without IPv6 loopback simply keeps the IPv4 listener.
        let ipv6 = TcpListener::bind(("::1", port)).await.ok();

        let (changed, _receiver) = watch::channel(0u64);
        let shared = Arc::new(Shared {
            acceptor,
            body: body.to_vec(),
            observation: Mutex::new(Observation::default()),
            changed,
        });

        let accept = [Some(listener), ipv6]
            .into_iter()
            .flatten()
            .map(|listener| {
                let connection = Arc::clone(&shared);
                tokio::spawn(async move {
                    loop {
                        let Ok((stream, _peer)) = listener.accept().await else {
                            return;
                        };
                        let connection = Arc::clone(&connection);
                        tokio::spawn(async move { connection.serve(stream).await });
                    }
                })
            })
            .collect();

        Self {
            port,
            shared,
            accept,
        }
    }

    /// Returns the port routes must name.
    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    /// Returns a snapshot of what the origin has observed.
    pub(crate) fn observation(&self) -> Observation {
        self.shared.snapshot()
    }

    /// Waits until at least one request arrived, and returns the most recent one.
    pub(crate) async fn wait_for_request(&self, budget: Duration) -> Exchange {
        self.wait_until(budget, |observation| !observation.requests.is_empty())
            .await
            .requests
            .last()
            .cloned()
            .expect("the condition required at least one request")
    }

    /// Waits until a client aborted a handshake.
    pub(crate) async fn wait_for_handshake_failure(&self, budget: Duration) -> Observation {
        self.wait_until(budget, |observation| observation.handshake_failures > 0)
            .await
    }

    /// Waits until `condition` holds, driven by the origin's own change notifications.
    async fn wait_until(
        &self,
        budget: Duration,
        condition: impl Fn(&Observation) -> bool,
    ) -> Observation {
        let mut changed = self.shared.changed.subscribe();
        let deadline = Instant::now() + budget;
        loop {
            let observation = self.shared.snapshot();
            if condition(&observation) {
                return observation;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "the TLS origin did not reach the expected state within {budget:?}: {observation:?}"
            );
            if timeout(remaining, changed.changed()).await.is_err() {
                let observation = self.shared.snapshot();
                panic!(
                    "the TLS origin did not reach the expected state within {budget:?}: {observation:?}"
                );
            }
        }
    }
}

impl Drop for TlsOrigin {
    fn drop(&mut self) {
        // Without this a listener would keep accepting for the rest of the test binary's life.
        for accept in &self.accept {
            accept.abort();
        }
    }
}

/// State shared by the accept loop and every connection it serves.
struct Shared {
    acceptor: TlsAcceptor,
    body: Vec<u8>,
    observation: Mutex<Observation>,
    /// Bumped on every observation; a subscriber wakes when the value changes.
    changed: watch::Sender<u64>,
}

impl Shared {
    async fn serve(self: Arc<Self>, stream: TcpStream) {
        // A failed handshake is the client refusing this certificate or this name: exactly what a
        // security scenario asserts, and the reason this origin exists rather than the HTTP mock.
        let Ok(mut stream) = self.acceptor.accept(stream).await else {
            self.record(|observation| observation.handshake_failures += 1);
            return;
        };
        self.record(|observation| observation.handshakes += 1);

        let Ok((head, overflow)) = read_head(&mut stream).await else {
            return;
        };
        let body = read_body(&mut stream, &head, overflow).await;
        self.record(|observation| {
            observation.requests.push(Exchange { head, body });
        });

        let response = response(&self.body);
        let _written = stream.write_all(&response).await;
        let _flushed = stream.flush().await;
        // One request per connection: the proxy opens another when it wants another, and no
        // scenario here depends on a reused connection.
        let _closed = stream.shutdown().await;
    }

    fn snapshot(&self) -> Observation {
        lock(&self.observation).clone()
    }

    fn record(&self, change: impl FnOnce(&mut Observation)) {
        {
            let mut observation = lock(&self.observation);
            change(&mut observation);
        }
        self.changed.send_modify(|generation| *generation += 1);
    }
}

/// Reads one request head, returning it plus whatever bytes of the body arrived with it.
async fn read_head<S: AsyncRead + Unpin>(stream: &mut S) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let overflow = buffer.split_off(end + 4);
            return Ok((buffer, overflow));
        }
    }
}

/// Reads the request body the head declared, starting from the bytes `overflow` already holds.
async fn read_body<S: AsyncRead + Unpin>(
    stream: &mut S,
    head: &[u8],
    overflow: Vec<u8>,
) -> Vec<u8> {
    let declared = content_length(head).unwrap_or(0);
    let mut body = overflow;
    while body.len() < declared {
        let mut chunk = vec![0_u8; declared - body.len()];
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => body.extend_from_slice(&chunk[..read]),
        }
    }
    body.truncate(declared);
    body
}

/// Returns the first value of header `name` in a head, case-insensitively.
fn header_value(head: &[u8], name: &str) -> Option<String> {
    let text = String::from_utf8_lossy(head);
    text.lines().find_map(|line| {
        let (existing, value) = line.split_once(':')?;
        existing
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_owned())
    })
}

/// Returns the `content-length` the head declared, case-insensitively.
fn content_length(head: &[u8]) -> Option<usize> {
    header_value(head, "content-length").and_then(|value| value.parse().ok())
}

/// Renders the one response this origin gives.
fn response(body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

/// Builds the server configuration from one certificate fixture pair.
///
/// The parse is `rustls-pemfile`'s — the same reader the production startup check uses for a trust
/// bundle — so a fixture that this test cannot read would also be one an operator could not configure.
fn server_config(certificate: &str, key: &str) -> ServerConfig {
    let mut certificate_reader = BufReader::new(
        File::open(fixture(certificate)).expect("the certificate fixture is readable"),
    );
    let certificates: Vec<_> = rustls_pemfile::certs(&mut certificate_reader)
        .collect::<Result<_, _>>()
        .expect("the certificate fixture is PEM");

    let mut key_reader =
        BufReader::new(File::open(fixture(key)).expect("the key fixture is readable"));
    let key = rustls_pemfile::private_key(&mut key_reader)
        .expect("the key fixture is readable")
        .expect("the key fixture holds a private key");

    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .expect("the fixture certificate and key match")
}

/// Locks the observation, recovering it when a previous holder panicked.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
