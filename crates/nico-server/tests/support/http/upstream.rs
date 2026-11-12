//! A hand-written mock upstream: a tokio TCP server that speaks raw HTTP/1.1.
//!
//! A proxy test needs an origin that can be made to misbehave on purpose — answer slowly, answer
//! half a response, never answer at all — and that records what it actually received. An HTTP server
//! crate would add a parser between the assertion and the bytes, so this one reads and writes the
//! wire format directly:
//!
//! - the instant the request head arrived, and the head bytes themselves;
//! - every request-body chunk with its arrival instant;
//! - whether the peer closed the connection, which is how a cancellation or a forced shutdown is
//!   observed on the upstream side.
//!
//! [`MockUpstream::wait_for_request`] and [`MockUpstream::wait_until_closed`] wait on those
//! observations, so a test never sleeps to "give it a moment" — it waits for the event it asserts.

mod reply;
mod wire;

use std::{
    fmt,
    net::SocketAddr,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use tokio::{
    net::{TcpListener, TcpStream},
    sync::watch,
    task::JoinHandle,
    time::timeout,
};

pub(crate) use self::reply::Reply;
use self::wire::{
    Wire, chunked_head, chunked_head_with, header_value, parse_headers, render_head, write_chunk,
    write_declared, write_response, write_response_with,
};

/// What one connection did, as far as the mock could observe it.
#[derive(Debug, Clone)]
pub(crate) struct ObservedRequest {
    head_at: Instant,
    head: Vec<u8>,
    headers: Vec<(String, String)>,
    body: Vec<(Instant, Vec<u8>)>,
}

impl ObservedRequest {
    /// Returns the first value of request header `name`, case-insensitively.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(existing, _value)| existing.eq_ignore_ascii_case(name))
            .map(|(_name, value)| value.as_str())
    }

    /// Returns the raw head bytes, request line included.
    pub(crate) fn head(&self) -> &[u8] {
        &self.head
    }

    /// Returns the instant the complete request head arrived.
    pub(crate) fn head_at(&self) -> Instant {
        self.head_at
    }

    /// Returns the instant the first request-body chunk arrived, when one did.
    pub(crate) fn first_body_at(&self) -> Option<Instant> {
        self.body.first().map(|(at, _bytes)| *at)
    }

    /// Returns the request body bytes received so far.
    pub(crate) fn body(&self) -> Vec<u8> {
        let mut body = Vec::new();
        for (_at, bytes) in &self.body {
            body.extend_from_slice(bytes);
        }
        body
    }

    /// Returns how many request-body bytes were received.
    pub(crate) fn body_len(&self) -> usize {
        self.body.iter().map(|(_at, bytes)| bytes.len()).sum()
    }
}

/// Everything the mock has observed across all its connections.
#[derive(Debug, Clone, Default)]
pub(crate) struct Observation {
    /// How many connections were accepted.
    pub(crate) connections: usize,
    /// How many connections the peer closed.
    pub(crate) closed: usize,
    /// One entry per request head, in arrival order.
    pub(crate) requests: Vec<ObservedRequest>,
}

impl Observation {
    /// Returns the most recent request, if one arrived.
    pub(crate) fn last(&self) -> Option<&ObservedRequest> {
        self.requests.last()
    }

    /// Describes the observation without quoting a body, so a failure message stays readable.
    pub(crate) fn summary(&self) -> String {
        let requests: Vec<String> = self
            .requests
            .iter()
            .map(|request| format!("{} body bytes", request.body_len()))
            .collect();
        format!(
            "{} connection(s), {} closed, requests: [{}]",
            self.connections,
            self.closed,
            requests.join(", ")
        )
    }
}

/// A mock upstream bound to one loopback port.
pub(crate) struct MockUpstream {
    address: SocketAddr,
    shared: Arc<Shared>,
    /// `None` while the origin is stopped: the port refuses connections and the observation stays.
    accept: Option<JoinHandle<()>>,
}

impl MockUpstream {
    /// Binds a port and starts answering with `reply`.
    pub(crate) async fn start(reply: Reply) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the mock upstream binds a loopback port");
        Self::serve(listener, reply).await
    }

    /// Starts answering at a specific address, so a scenario can bring the same peer back.
    ///
    /// A load-balancing scenario has to make one member of a fleet disappear and reappear, and the
    /// document names the members by address: the second instance must therefore bind the port the
    /// first one released, which is why the address is an argument here.
    pub(crate) async fn start_at(address: SocketAddr, reply: Reply) -> Self {
        let listener = TcpListener::bind(address)
            .await
            .unwrap_or_else(|error| panic!("the mock upstream rebinds {address}: {error}"));
        Self::serve(listener, reply).await
    }

    /// Starts the accept loop on an already-bound listener.
    async fn serve(listener: TcpListener, reply: Reply) -> Self {
        let address = listener
            .local_addr()
            .expect("the mock upstream has an address");
        let (changed, _receiver) = watch::channel(0u64);
        let shared = Arc::new(Shared {
            reply,
            observation: Mutex::new(Observation::default()),
            changed,
        });
        let accept = Self::accept(listener, &shared);
        Self {
            address,
            shared,
            accept: Some(accept),
        }
    }

    /// Spawns the accept loop, sharing the observation with the running origin.
    fn accept(listener: TcpListener, shared: &Arc<Shared>) -> JoinHandle<()> {
        let accepted = Arc::clone(shared);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _peer)) = listener.accept().await else {
                    return;
                };
                let connection = Arc::clone(&accepted);
                tokio::spawn(async move { connection.serve(stream).await });
            }
        })
    }

    /// Stops accepting, keeping what was observed so far.
    ///
    /// Awaited rather than merely aborted: the port must be free before a test can bind it again, and
    /// the listener lives inside the task that has to be dropped first. Without the await, a restart
    /// would race the release and the failure would look like a flaky fixture.
    pub(crate) async fn stop(&mut self) {
        if let Some(accept) = self.accept.take() {
            accept.abort();
            let _cancelled = accept.await;
        }
    }

    /// Returns the address routes must name.
    pub(crate) fn address(&self) -> SocketAddr {
        self.address
    }

    /// Returns a snapshot of what the mock has observed.
    pub(crate) fn observation(&self) -> Observation {
        self.shared.snapshot()
    }

    /// Waits until at least one request head has arrived, and returns it.
    pub(crate) async fn wait_for_request(&self, budget: Duration) -> ObservedRequest {
        self.wait_until(budget, |observation| !observation.requests.is_empty())
            .await
            .last()
            .cloned()
            .expect("the condition required at least one request")
    }

    /// Waits until the peer closed every connection it opened.
    pub(crate) async fn wait_until_closed(
        &self,
        budget: Duration,
        connections: usize,
    ) -> Observation {
        self.wait_until(budget, |observation| observation.closed >= connections)
            .await
    }

    /// Waits until `condition` holds, driven by the mock's own change notifications.
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
                "the mock upstream did not reach the expected state within {budget:?}: {}",
                observation.summary()
            );
            if timeout(remaining, changed.changed()).await.is_err() {
                let observation = self.shared.snapshot();
                panic!(
                    "the mock upstream did not reach the expected state within {budget:?}: {}",
                    observation.summary()
                );
            }
        }
    }
}

impl Drop for MockUpstream {
    fn drop(&mut self) {
        // Without this the listener would keep accepting for the rest of the test binary's life.
        if let Some(accept) = self.accept.take() {
            accept.abort();
        }
    }
}

/// State shared by the accept loop and every connection it serves.
struct Shared {
    reply: Reply,
    observation: Mutex<Observation>,
    /// Bumped on every observation; a subscriber wakes when the value changes.
    changed: watch::Sender<u64>,
}

impl Shared {
    async fn serve(self: Arc<Self>, stream: TcpStream) {
        self.record(|observation| observation.connections += 1);
        let mut wire = Wire::new(stream);

        let head = match wire.read_head().await {
            Ok(Some(head)) => head,
            // The peer left before a complete head: nothing to answer and nothing to record.
            Ok(None) | Err(_) => {
                self.record(|observation| observation.closed += 1);
                return;
            }
        };
        let head_at = Instant::now();
        let headers = parse_headers(&head);
        self.record(|observation| {
            observation.requests.push(ObservedRequest {
                head_at,
                head: head.clone(),
                headers: headers.clone(),
                body: Vec::new(),
            });
        });

        match self.reply.clone() {
            Reply::ContinueThen { body } => {
                // One distinctive header, so the client-side assertion can tell "forwarded verbatim"
                // from "forwarded with something added".
                let interim = render_head(100, &[("x-mock-interim", "continue")]);
                if wire.write(&interim).await.is_ok() {
                    read_request_body(&mut wire, &headers, &self).await;
                    let _written = write_response(&mut wire, &body, usize::MAX).await;
                }
            }
            Reply::StallBeforeHead => self.wait_for_close(&mut wire).await,
            Reply::StallAfterHead => {
                if wire.write(&chunked_head()).await.is_ok() {
                    self.wait_for_close(&mut wire).await;
                }
            }
            Reply::StallMidStream { first_chunk } => {
                if wire.write(&chunked_head()).await.is_ok()
                    && write_chunk(&mut wire, &first_chunk).await.is_ok()
                {
                    self.wait_for_close(&mut wire).await;
                }
            }
            Reply::Echo => {
                let body = read_request_body(&mut wire, &headers, &self).await;
                let _written = write_response(&mut wire, &body, usize::MAX).await;
            }
            Reply::Body { body, chunk } => {
                read_request_body(&mut wire, &headers, &self).await;
                let _written = write_response(&mut wire, &body, chunk).await;
            }
            Reply::Response {
                status,
                headers: response_headers,
                body,
                chunk,
            } => {
                read_request_body(&mut wire, &headers, &self).await;
                let _written =
                    write_response_with(&mut wire, status, &response_headers, &body, chunk).await;
            }
            Reply::Declared {
                status,
                content_length,
                body,
                chunk,
            } => {
                read_request_body(&mut wire, &headers, &self).await;
                let _written =
                    write_declared(&mut wire, status, content_length, &body, chunk).await;
            }
            Reply::StreamWithHeaders {
                headers: response_headers,
                events,
                gap,
            } => {
                read_request_body(&mut wire, &headers, &self).await;
                if wire
                    .write(&chunked_head_with(&response_headers))
                    .await
                    .is_err()
                {
                    return;
                }
                for (index, event) in events.iter().enumerate() {
                    if index > 0 {
                        tokio::time::sleep(gap).await;
                    }
                    if write_chunk(&mut wire, event).await.is_err() {
                        return;
                    }
                }
                let _finished = wire.write(b"0\r\n\r\n").await;
            }
            Reply::StreamThenDie {
                headers: response_headers,
                first_chunk,
            } => {
                read_request_body(&mut wire, &headers, &self).await;
                if wire
                    .write(&chunked_head_with(&response_headers))
                    .await
                    .is_err()
                {
                    return;
                }
                // Close without `0\r\n\r\n`: the client's stream ends mid-message, which is exactly
                // what a provider that died after its head produces. A failed write means the
                // gateway already went away, which is the same ending.
                let _ = write_chunk(&mut wire, &first_chunk).await;
            }
            Reply::Stream { events, gap } => {
                read_request_body(&mut wire, &headers, &self).await;
                if wire.write(&chunked_head()).await.is_err() {
                    return;
                }
                for (index, event) in events.iter().enumerate() {
                    if index > 0 {
                        tokio::time::sleep(gap).await;
                    }
                    if write_chunk(&mut wire, event).await.is_err() {
                        return;
                    }
                }
                let _finished = wire.write(b"0\r\n\r\n").await;
            }
        }
    }

    /// Reads until the peer closes, and records the closure.
    async fn wait_for_close(&self, wire: &mut Wire) {
        let _unread = wire.read_to_end().await;
        self.record(|observation| observation.closed += 1);
    }

    fn snapshot(&self) -> Observation {
        lock(&self.observation).clone()
    }

    fn record(&self, update: impl FnOnce(&mut Observation)) {
        {
            let mut observation = lock(&self.observation);
            update(&mut observation);
        }
        // `send_modify` always notifies, even when no subscriber is listening.
        self.changed.send_modify(|seq| *seq += 1);
    }
}

impl fmt::Debug for Shared {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Shared")
            .field("reply", &self.reply)
            .finish_non_exhaustive()
    }
}

/// Locks the observation, recovering it when a previous holder panicked.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Reads the request body with the framing the head declared, recording every chunk.
///
/// Returns the decoded bytes, so a reply that echoes the request does not have to read the shared
/// observation back.
async fn read_request_body(
    wire: &mut Wire,
    headers: &[(String, String)],
    shared: &Shared,
) -> Vec<u8> {
    let declared =
        header_value(headers, "content-length").and_then(|value| value.parse::<u64>().ok());
    let chunked = header_value(headers, "transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"));

    let mut chunks = Vec::new();
    let result = match (declared, chunked) {
        // A body smaller than one read may already be buffered behind the head.
        (Some(0), _) => Ok(()),
        (Some(length), _) => wire.read_exact(length as usize, &mut chunks).await,
        (None, true) => wire.read_chunked(&mut chunks).await,
        (None, false) => Ok(()),
    };
    let mut body = Vec::new();
    for (at, bytes) in chunks {
        body.extend_from_slice(&bytes);
        shared.record(|observation| {
            if let Some(request) = observation.requests.last_mut() {
                request.body.push((at, bytes));
            }
        });
    }
    // A framing failure means the peer went away mid-body; the bytes read so far are the answer.
    let _result = result;
    body
}
