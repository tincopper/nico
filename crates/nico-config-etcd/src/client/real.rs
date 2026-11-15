//! The real gRPC client: the [`EtcdClient`] seam implemented over the `etcd-client` crate.
//!
//! Everything here is mapping between two vocabularies — etcd's wire API and the seam's typed
//! protocol. No policy lives here: filtering, resync folding and size budgets are the adapter's
//! (see `source.rs`), tested against the scripted client. This module is tested end-to-end against
//! an in-process mock server speaking the same vendored protos, which is what verifies the
//! *mapping* — status classification, inclusive revisions, cancellations — on a real gRPC wire
//! without an etcd environment.
//!
//! Three facts about the real wire, stated where they are mapped:
//!
//! - **the subscription is established before this code sees anything**: `etcd-client`'s
//!   `Client::watch` sends the watch request, awaits the create confirmation itself, and only then
//!   returns the stream — so the seam's `Established` outcome is never produced over this client;
//! - **`fragment` is not surfaced**: fragmented watches arrive as ordinary responses with subsets of
//!   events, which the adapter already processes one by one;
//! - **operations need `&mut`**: `etcd_client::Client` multiplexes over one channel but its methods
//!   take `&mut self`, so the client sits behind a mutex that is never held across an `.await` the
//!   caller sees.

use bytes::Bytes;
use etcd_client::{Client, GetOptions, WatchOptions};
use tokio::sync::Mutex;

use crate::{
    client::{EtcdClient, EtcdError, KvEntry, WatchEvent, WatchOutcome, WatchStream},
    source::WATCH_CAPACITY,
};

/// The real client: one multiplexed connection to the etcd endpoints.
///
/// TLS and endpoint failover are the client's own configuration — beneath this seam, exactly where
/// the adapter contract wants them.
pub struct GrpcEtcdClient {
    client: Mutex<Client>,
}

impl std::fmt::Debug for GrpcEtcdClient {
    /// Names the connection without exposing endpoints or credentials beyond a count.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GrpcEtcdClient")
            .finish_non_exhaustive()
    }
}

impl GrpcEtcdClient {
    /// Connects to the etcd endpoints and returns a client speaking the seam.
    ///
    /// # Errors
    /// Returns [`EtcdError::Connect`] when an endpoint is not a valid address, or the server cannot
    /// be reached.
    pub async fn connect(endpoints: &[String]) -> Result<Self, EtcdError> {
        let client = Client::connect(endpoints, None).await.map_err(classify)?;
        Ok(Self {
            client: Mutex::new(client),
        })
    }
}

/// Maps one client failure to the seam's two reasons: cannot reach, or refused.
///
/// A gRPC status carries the code that tells them apart: the server rejecting the caller is auth,
/// anything else about the transport is a connection failure. Application errors the server sent
/// inside a well-formed response (key not found, for instance) do not arrive here — those are
/// responses, and the adapter interprets them.
fn classify(error: etcd_client::Error) -> EtcdError {
    use etcd_client::Error as Wire;

    match error {
        Wire::GRpcStatus(status) => match status.code() {
            tonic::Code::Unauthenticated | tonic::Code::PermissionDenied => EtcdError::Auth {
                message: status.message().to_owned(),
            },
            _ => EtcdError::Connect {
                message: format!("{}: {}", status.code(), status.message()),
            },
        },
        // Every other wire failure is about reaching the server, whatever layer raised it.
        other => EtcdError::Connect {
            message: other.to_string(),
        },
    }
}

/// Reads one stored document out of a get response.
///
/// The adapter watches exactly one key that holds one complete document, so the first matching kv is
/// the document and an empty response means the key does not exist.
fn entry_from(response: &etcd_client::GetResponse) -> Option<KvEntry> {
    response.kvs().first().map(|kv| KvEntry {
        value: Bytes::copy_from_slice(kv.value()),
        mod_revision: u64::try_from(kv.mod_revision()).unwrap_or(0),
    })
}

/// Maps one watch response to the seam's outcome, or `None` when nothing is deliverable.
///
/// Three facts about the real protocol, stated where they are mapped:
///
/// - **the create confirmation never reaches this code**: `etcd-client` awaits it inside
///   `Client::watch` (its own tests assert `created()`), so the first response this function sees is
///   an event or a cancellation — the seam's `Established` outcome is not produced over this client;
/// - **a cancellation names its reason**: a compacted watch arrives as `canceled` with
///   `compact_revision` set — the adapter answers it with a full reload;
/// - **`fragment` is not surfaced**: fragmented watches arrive as ordinary responses with subsets of
///   events, which the adapter already processes one by one.
fn outcome_from(response: &etcd_client::WatchResponse) -> Option<WatchOutcome> {
    if response.canceled() {
        let compact_revision = u64::try_from(response.compact_revision()).ok();
        // A canceled watch without a compaction revision (the caller canceled it) still cannot be
        // resumed incrementally: the seam folds it into a resync either way.
        return Some(WatchOutcome::Canceled { compact_revision });
    }
    let events: Vec<WatchEvent> = response
        .events()
        .iter()
        .filter_map(|event| {
            let kv = event.kv()?;
            let mod_revision = u64::try_from(kv.mod_revision()).unwrap_or(0);
            Some(match event.event_type() {
                etcd_client::EventType::Put => WatchEvent::Put {
                    value: Bytes::copy_from_slice(kv.value()),
                    mod_revision,
                },
                etcd_client::EventType::Delete => WatchEvent::Delete,
            })
        })
        .collect();
    if events.is_empty() {
        // A response with neither events nor a cancel carries nothing deliverable.
        return None;
    }
    Some(WatchOutcome::Events { events })
}

#[async_trait::async_trait]
impl EtcdClient for GrpcEtcdClient {
    async fn get(&self, key: &str) -> Result<Option<KvEntry>, EtcdError> {
        let response = {
            let mut client = self.client.lock().await;
            client
                .get(key, Some(GetOptions::new()))
                .await
                .map_err(classify)?
        };
        Ok(entry_from(&response))
    }

    async fn watch(&self, key: &str, start_revision: u64) -> Result<WatchStream, EtcdError> {
        // Inclusive start: the subscription begins at `start_revision`, and the adapter's filtering
        // drops anything at or before the revision it already holds. The lock is released before
        // the stream is consumed: the watch lives on its own task from here.
        let mut stream = {
            let options =
                WatchOptions::new().with_start_revision(i64::try_from(start_revision).unwrap_or(1));
            let mut client = self.client.lock().await;
            let (_watcher, stream) = client.watch(key, Some(options)).await.map_err(classify)?;
            stream
        };

        let (sender, receiver) = tokio::sync::mpsc::channel(WATCH_CAPACITY);
        tokio::spawn(async move {
            loop {
                let outcome = match stream.message().await {
                    Ok(Some(response)) => outcome_from(&response),
                    Ok(None) => {
                        // The watch stream ended: the server closed it, which the adapter answers
                        // as "cannot be read" and the pipeline re-establishes.
                        let _ = sender
                            .send(Err(EtcdError::Connect {
                                message: "the etcd watch stream ended".into(),
                            }))
                            .await;
                        return;
                    }
                    Err(error) => {
                        let _ = sender.send(Err(classify(error))).await;
                        return;
                    }
                };
                let Some(outcome) = outcome else {
                    continue;
                };
                if sender.send(Ok(outcome)).await.is_err() {
                    return;
                }
            }
        });
        Ok(receiver)
    }
}
