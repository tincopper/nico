//! An in-process etcd: the "environment" the real client is verified against.
//!
//! It serves the **official** etcd v3 gRPC protocol — the server half generated from the same
//! vendored protos the real client's wire speaks — backed by an in-memory store that keeps the full
//! revision history of one key. Tests hold its handle and drive the store directly (put, delete,
//! compact, require auth), so every scenario the roadmap asked to verify has a deterministic way to
//! happen, and the wire between the real client and this server is a real gRPC connection over
//! loopback.
//!
//! What it deliberately does **not** model: TLS, leases, transactions, multi-endpoint failover, and
//! anything else a real cluster does that the adapter contract does not observe. The adapter folds
//! what it cannot understand into resyncs and unavailability; this server produces those situations
//! and nothing else.

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};

use crate::client::proto::pb::{
    etcdserverpb::{
        RangeRequest, RangeResponse, ResponseHeader, WatchRequest, WatchResponse,
        kv_server::{Kv, KvServer},
        watch_request::RequestUnion,
        watch_server::{Watch, WatchServer},
    },
    mvccpb::{Event, KeyValue, event::EventType},
};

/// One revision of the watched key: the document bytes stored at that revision.
///
/// An empty value is the tombstone a delete leaves behind.
type History = Vec<(i64, Vec<u8>)>;

/// Unlocks a poisoned lock by keeping the state: a test harness with a panicked test is already a
/// failed test, so the data is safe to read.
fn held<'a, T>(
    lock: Result<MutexGuard<'a, T>, PoisonError<MutexGuard<'a, T>>>,
) -> MutexGuard<'a, T> {
    lock.unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// One live change the store broadcasts to its watch streams.
#[derive(Debug, Clone)]
struct LiveChange {
    revision: i64,
    value: Vec<u8>,
}

/// The shared state one server instance serves, driven directly by its tests.
#[derive(Debug)]
pub(crate) struct StoreState {
    /// The full history of the key, ascending by revision. An empty value is a delete tombstone.
    history: Mutex<History>,
    /// The revision the store hands out next; one past the newest history entry.
    next_revision: Mutex<i64>,
    /// When set, every operation is refused with `permission_denied`.
    require_auth: Mutex<bool>,
    /// When set, every watch starting at or before this revision is canceled naming it.
    compact_to: Mutex<Option<i64>>,
    /// The live tail every watch stream subscribes to.
    live: broadcast::Sender<LiveChange>,
}

impl StoreState {
    /// Builds an empty store: no history, nothing compacted, no auth required.
    fn new() -> Self {
        let (live, _initial) = broadcast::channel(64);
        Self {
            history: Mutex::new(History::new()),
            next_revision: Mutex::new(0),
            require_auth: Mutex::new(false),
            compact_to: Mutex::new(None),
            live,
        }
    }

    /// Stores a new revision of the key, returns its mod revision, and broadcasts the change.
    pub(crate) fn put(&self, document: &[u8]) -> i64 {
        self.append(document.to_vec())
    }

    /// Deletes the key: a tombstone revision, broadcast like any other change.
    pub(crate) fn delete(&self) -> i64 {
        self.append(Vec::new())
    }

    fn append(&self, value: Vec<u8>) -> i64 {
        let revision = {
            let mut next = held(self.next_revision.lock());
            *next += 1;
            *next
        };
        held(self.history.lock()).push((revision, value.clone()));
        // No receivers is the normal case before a watch is established; the change is still in
        // the history for replay.
        let _ = self.live.send(LiveChange { revision, value });
        revision
    }

    /// Sets the auth requirement. Every operation afterwards is refused until cleared.
    pub(crate) fn set_require_auth(&self, required: bool) {
        *held(self.require_auth.lock()) = required;
    }

    /// Compacts history at or below `revision`: every watch starting there is canceled naming it,
    /// exactly as the roadmap's semantics state.
    pub(crate) fn compact(&self, revision: i64) {
        *held(self.compact_to.lock()) = Some(revision);
    }

    fn auth_refused(&self) -> bool {
        *held(self.require_auth.lock())
    }

    fn current_revision(&self) -> i64 {
        *held(self.next_revision.lock())
    }

    fn history(&self) -> History {
        held(self.history.lock()).clone()
    }

    fn compact_to(&self) -> Option<i64> {
        *held(self.compact_to.lock())
    }

    fn subscribe(&self) -> broadcast::Receiver<LiveChange> {
        self.live.subscribe()
    }
}

/// One running server: its address, and the store handle its tests drive.
pub(crate) struct MockServer {
    address: SocketAddr,
    state: Arc<StoreState>,
}

impl MockServer {
    /// Starts the KV and Watch services on an ephemeral loopback port.
    ///
    /// The listener is handed to the server as an incoming stream so the ephemeral port is bound
    /// exactly once — no free-port race between picking the address and serving it.
    ///
    /// # Errors
    /// Returns an error when the loopback port cannot be bound or reported.
    pub(crate) async fn start() -> std::io::Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let state = Arc::new(StoreState::new());
        let kv = KvServer::new(MockKv {
            state: Arc::clone(&state),
        });
        let watch = WatchServer::new(MockWatch {
            state: Arc::clone(&state),
        });
        tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(kv)
                .add_service(watch)
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await;
        });
        Ok(Self { address, state })
    }

    /// Returns the endpoint the real client connects to.
    pub(crate) fn endpoint(&self) -> String {
        format!("http://{}", self.address)
    }

    /// Returns the store handle the test drives.
    pub(crate) fn state(&self) -> Arc<StoreState> {
        Arc::clone(&self.state)
    }
}

/// The in-memory KV service: `range` reads the newest revision.
#[derive(Debug)]
struct MockKv {
    state: Arc<StoreState>,
}

#[tonic::async_trait]
impl Kv for MockKv {
    async fn range(
        &self,
        request: tonic::Request<RangeRequest>,
    ) -> Result<tonic::Response<RangeResponse>, tonic::Status> {
        if self.state.auth_refused() {
            return Err(tonic::Status::permission_denied(
                "authentication is required",
            ));
        }
        let revision = self.state.current_revision();
        let kvs = self
            .state
            .history()
            .last()
            // A tombstone as the newest revision means the key does not exist: nothing to return.
            .filter(|(_, value)| !value.is_empty())
            .map(|(mod_revision, value)| KeyValue {
                key: request.get_ref().key.clone(),
                value: value.clone(),
                mod_revision: *mod_revision,
                ..Default::default()
            })
            .into_iter()
            .collect();
        Ok(tonic::Response::new(RangeResponse {
            header: Some(ResponseHeader {
                revision,
                ..Default::default()
            }),
            kvs,
            ..Default::default()
        }))
    }

    // The adapter drives exactly one KV RPC; the rest of the surface is refused loudly rather than
    // silently succeeding.
    async fn put(
        &self,
        _request: tonic::Request<crate::client::proto::pb::etcdserverpb::PutRequest>,
    ) -> Result<tonic::Response<crate::client::proto::pb::etcdserverpb::PutResponse>, tonic::Status>
    {
        Err(tonic::Status::unimplemented(
            "the mock etcd only serves range",
        ))
    }

    async fn delete_range(
        &self,
        _request: tonic::Request<crate::client::proto::pb::etcdserverpb::DeleteRangeRequest>,
    ) -> Result<
        tonic::Response<crate::client::proto::pb::etcdserverpb::DeleteRangeResponse>,
        tonic::Status,
    > {
        Err(tonic::Status::unimplemented(
            "the mock etcd only serves range",
        ))
    }

    async fn txn(
        &self,
        _request: tonic::Request<crate::client::proto::pb::etcdserverpb::TxnRequest>,
    ) -> Result<tonic::Response<crate::client::proto::pb::etcdserverpb::TxnResponse>, tonic::Status>
    {
        Err(tonic::Status::unimplemented(
            "the mock etcd only serves range",
        ))
    }

    async fn compact(
        &self,
        _request: tonic::Request<crate::client::proto::pb::etcdserverpb::CompactionRequest>,
    ) -> Result<
        tonic::Response<crate::client::proto::pb::etcdserverpb::CompactionResponse>,
        tonic::Status,
    > {
        Err(tonic::Status::unimplemented(
            "the mock etcd only serves range",
        ))
    }
}

/// The in-memory Watch service: one streamed subscription — confirmation, history, live tail.
#[derive(Debug)]
struct MockWatch {
    state: Arc<StoreState>,
}

#[tonic::async_trait]
impl Watch for MockWatch {
    type WatchStream = ReceiverStream<Result<WatchResponse, tonic::Status>>;

    async fn watch(
        &self,
        request: tonic::Request<tonic::Streaming<WatchRequest>>,
    ) -> Result<tonic::Response<Self::WatchStream>, tonic::Status> {
        if self.state.auth_refused() {
            return Err(tonic::Status::permission_denied(
                "authentication is required",
            ));
        }
        // The client's first request-stream message creates the watcher — the shape `etcd-client`
        // sends, key and start_revision riding in the creation.
        let creation = match request.into_inner().message().await? {
            Some(WatchRequest {
                request_union: Some(RequestUnion::CreateRequest(creation)),
            }) => creation,
            _ => {
                return Err(tonic::Status::invalid_argument(
                    "the first watch request must create a watcher",
                ));
            }
        };
        let key = creation.key;
        // Inclusive start, exactly as the seam documents it; an unset start means "now".
        let start = if creation.start_revision > 0 {
            creation.start_revision
        } else {
            self.state.current_revision() + 1
        };

        // Subscribe before anything is sent: a change that lands while the history replays is
        // delivered live too, and the adapter's revision filter absorbs the duplicate.
        let mut live = self.state.subscribe();
        let state = Arc::clone(&self.state);
        let (sender, receiver) = mpsc::channel(64);

        tokio::spawn(async move {
            // Real etcd confirms the creation before any events; `etcd-client` consumes that
            // confirmation inside its `watch` call, so the adapter never sees it.
            let created = WatchResponse {
                header: Some(ResponseHeader {
                    revision: state.current_revision(),
                    ..Default::default()
                }),
                created: true,
                ..Default::default()
            };
            if sender.send(Ok(created)).await.is_err() {
                return;
            }

            // A start inside the compacted region cannot be served gaplessly: real etcd cancels
            // the watcher naming the compaction revision, and the adapter answers with a reload.
            if let Some(compact_to) = state.compact_to().filter(|cut| *cut >= start) {
                let _ = sender
                    .send(Ok(WatchResponse {
                        canceled: true,
                        compact_revision: compact_to,
                        ..Default::default()
                    }))
                    .await;
                return;
            }

            // Replay the history the subscription asked for, one event per response.
            for (mod_revision, value) in state.history() {
                if mod_revision < start {
                    continue;
                }
                let response = WatchResponse {
                    events: vec![Event {
                        r#type: EventType::Put.into(),
                        kv: Some(KeyValue {
                            key: key.clone(),
                            value,
                            mod_revision,
                            ..Default::default()
                        }),
                        ..Default::default()
                    }],
                    ..Default::default()
                };
                if sender.send(Ok(response)).await.is_err() {
                    return;
                }
            }

            // The live tail: every change from here on streams until the consumer leaves.
            loop {
                match live.recv().await {
                    Ok(change) => {
                        // A tombstone is a delete event: the deleted key rides in the event with
                        // its deletion revision, the value stripped — real etcd's own shape.
                        let (event_type, value) = if change.value.is_empty() {
                            (EventType::Delete, Vec::new())
                        } else {
                            (EventType::Put, change.value)
                        };
                        let response = WatchResponse {
                            events: vec![Event {
                                r#type: event_type.into(),
                                kv: Some(KeyValue {
                                    key: key.clone(),
                                    value,
                                    mod_revision: change.revision,
                                    ..Default::default()
                                }),
                                ..Default::default()
                            }],
                            ..Default::default()
                        };
                        if sender.send(Ok(response)).await.is_err() {
                            return;
                        }
                    }
                    // The subscriber fell too far behind: the stream can no longer be trusted to
                    // be gapless, which real etcd answers the same way — a cancellation.
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let _ = sender
                            .send(Ok(WatchResponse {
                                canceled: true,
                                ..Default::default()
                            }))
                            .await;
                        return;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });

        Ok(tonic::Response::new(ReceiverStream::new(receiver)))
    }
}
