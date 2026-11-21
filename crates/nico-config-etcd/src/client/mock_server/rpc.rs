//! The two services the adapter drives, spoken over the official etcd v3 protos.
//!
//! Both are thin: they read [`StoreState`] and translate, and they produce the wire shape a real
//! etcd produces — a `created` confirmation before any event, one event per response, a tombstone
//! for a delete, a cancellation naming the compaction revision. A client that needs something here
//! that a real cluster does not do would be a client this repository could not ship.
//!
//! Everything the adapter never calls is refused loudly rather than silently succeeding: a mock that
//! answered `put` with an empty response would let a test pass on a path nothing implemented.

use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;

use crate::client::{
    mock_server::store::{LiveChange, StoreState},
    proto::pb::{
        etcdserverpb::{
            CompactionRequest, CompactionResponse, DeleteRangeRequest, DeleteRangeResponse,
            PutRequest, PutResponse, RangeRequest, RangeResponse, ResponseHeader, TxnRequest,
            TxnResponse, WatchRequest, WatchResponse, kv_server::Kv, watch_request::RequestUnion,
            watch_server::Watch,
        },
        mvccpb::{Event, KeyValue, event::EventType},
    },
};

/// The in-memory KV service: `range` reads the newest revision.
#[derive(Debug)]
pub(super) struct MockKv {
    pub(super) state: Arc<StoreState>,
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
        _request: tonic::Request<PutRequest>,
    ) -> Result<tonic::Response<PutResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented(
            "the mock etcd only serves range",
        ))
    }

    async fn delete_range(
        &self,
        _request: tonic::Request<DeleteRangeRequest>,
    ) -> Result<tonic::Response<DeleteRangeResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented(
            "the mock etcd only serves range",
        ))
    }

    async fn txn(
        &self,
        _request: tonic::Request<TxnRequest>,
    ) -> Result<tonic::Response<TxnResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented(
            "the mock etcd only serves range",
        ))
    }

    async fn compact(
        &self,
        _request: tonic::Request<CompactionRequest>,
    ) -> Result<tonic::Response<CompactionResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented(
            "the mock etcd only serves range",
        ))
    }
}

/// The in-memory Watch service: one streamed subscription — confirmation, history, live tail.
#[derive(Debug)]
pub(super) struct MockWatch {
    pub(super) state: Arc<StoreState>,
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
                        let response = live_response(&key, change);
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

/// Translates one live change into the response real etcd would send.
/// A tombstone is a delete event: the deleted key rides in the event with its deletion revision and
/// the value stripped, which is real etcd's own shape.
fn live_response(key: &[u8], change: LiveChange) -> WatchResponse {
    let (event_type, value) = if change.value.is_empty() {
        (EventType::Delete, Vec::new())
    } else {
        (EventType::Put, change.value)
    };
    WatchResponse {
        events: vec![Event {
            r#type: event_type.into(),
            kv: Some(KeyValue {
                key: key.to_vec(),
                value,
                mod_revision: change.revision,
                ..Default::default()
            }),
            ..Default::default()
        }],
        ..Default::default()
    }
}
