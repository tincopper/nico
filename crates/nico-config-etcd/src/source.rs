//! The `ConfigSource` implementation for one etcd key.
//!
//! Everything here is semantics over the [`EtcdClient`] seam: load maps a `get` to a snapshot,
//! watch maps the typed protocol to config events, and every failure folds into the contract's
//! "cannot be read". Nothing knows a network exists.

use std::{
    cmp::Ordering as Cmp,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::Stream;
use nico_config::{
    ConfigEvent, ConfigSource, ConfigSourceError, ConfigVersion, ConfigWatch, Revision,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::client::{EtcdClient, KvEntry, WatchEvent, WatchOutcome};

/// The source name every unavailable error carries.
const SOURCE_NAME: &str = "etcd";

/// The watch channel's bound. Small on purpose: a consumer that falls behind is answered with one
/// resync instead of an unbounded history, and the pipeline reloads the whole document anyway.
pub(crate) const WATCH_CAPACITY: usize = 16;

/// One item of the watch stream.
type WatchItem = Result<ConfigEvent, ConfigSourceError>;

/// The `Stream` half of the watch, cancelling the producing task on drop.
struct WatchBridge {
    receiver: mpsc::Receiver<WatchItem>,
    token: CancellationToken,
}

impl Stream for WatchBridge {
    type Item = WatchItem;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<WatchItem>> {
        // `WatchBridge` is `Unpin`: both fields are plain owned values.
        self.get_mut().receiver.poll_recv(context)
    }
}

impl Drop for WatchBridge {
    fn drop(&mut self) {
        self.token.cancel();
    }
}

/// One etcd-backed configuration source: a key, a size budget, and a client seam.
///
/// The key holds one **complete** configuration document; there are no patches and no per-field
/// keys. The client is shared through an `Arc` so `load` and `watch` can run against the same
/// connection the real implementation maintains.
pub struct EtcdConfigSource {
    client: Arc<dyn EtcdClient>,
    key: Arc<str>,
    max_bytes: usize,
}

impl EtcdConfigSource {
    /// Builds a source reading `key`, refusing documents above `max_bytes`, through `client`.
    #[must_use]
    pub fn new(key: impl Into<Arc<str>>, max_bytes: usize, client: Arc<dyn EtcdClient>) -> Self {
        Self {
            client,
            key: key.into(),
            max_bytes,
        }
    }

    /// Checks the size bound and the encoding of a stored document.
    fn checked(&self, value: &Bytes) -> Result<Bytes, ConfigSourceError> {
        checked(value, self.max_bytes)
    }

    /// Maps one stored entry to the config version it is.
    fn version(&self, entry: &KvEntry) -> Result<ConfigVersion, ConfigSourceError> {
        let bytes = self.checked(&entry.value)?;
        Ok(ConfigVersion::new(
            Revision::comparable(entry.mod_revision),
            bytes,
        ))
    }

    /// Maps one client failure to the contract's unavailable answer.
    fn unavailable<E: std::error::Error + Send + Sync + 'static>(
        &self,
        error: E,
    ) -> ConfigSourceError {
        ConfigSourceError::Unavailable {
            source_name: SOURCE_NAME,
            source: Box::new(error),
        }
    }

    /// The answer for a key that no longer exists.
    fn deleted() -> ConfigSourceError {
        ConfigSourceError::Unavailable {
            source_name: SOURCE_NAME,
            source: "the configuration key does not exist".into(),
        }
    }
}

/// Checks a value against the size bound and the encoding.
fn checked(value: &Bytes, max_bytes: usize) -> Result<Bytes, ConfigSourceError> {
    if value.len() > max_bytes {
        return Err(ConfigSourceError::TooLarge {
            size: value.len(),
            limit: max_bytes,
        });
    }
    std::str::from_utf8(value).map_err(|_| ConfigSourceError::NotUtf8)?;
    Ok(value.clone())
}

#[async_trait]
impl ConfigSource for EtcdConfigSource {
    fn name(&self) -> &'static str {
        SOURCE_NAME
    }

    async fn load(&self) -> Result<ConfigVersion, ConfigSourceError> {
        // A missing key is not an empty document: the configuration source has stopped existing, and
        // publishing emptiness would overwrite the last known good snapshot with nothing.
        let entry = self
            .client
            .get(&self.key)
            .await
            .map_err(|error| self.unavailable(error))?
            .ok_or_else(Self::deleted)?;
        self.version(&entry)
    }

    async fn watch(&self, from: Option<Revision>) -> Result<ConfigWatch, ConfigSourceError> {
        // Inclusive start: the load already read `rev`, so the subscription resumes one past it and
        // the stream delivers every revision after without repeating the one in hand.
        let start = from.map_or(1, |revision| revision.seq() + 1);
        let mut stream = self
            .client
            .watch(&self.key, start)
            .await
            .map_err(|error| self.unavailable(error))?;

        let (sender, receiver) = mpsc::channel(WATCH_CAPACITY);
        let token = CancellationToken::new();
        let task_token = token.clone();
        let max_bytes = self.max_bytes;
        // The revision in hand: everything at or before it is history this request already carried.
        let mut last = start.saturating_sub(1);

        // The handle is detached on purpose: the stream ends when the source stops or the receiver
        // is dropped, and nothing in the source needs to join the task.
        let _handle = tokio::spawn(async move {
            // Sends one item, answering whether the consumer is still listening.
            async fn send(
                sender: &mpsc::Sender<WatchItem>,
                token: &CancellationToken,
                item: WatchItem,
            ) -> bool {
                tokio::select! {
                    result = sender.send(item) => result.is_ok(),
                    () = token.cancelled() => false,
                }
            }

            loop {
                let outcome = match stream.recv().await {
                    Some(Ok(outcome)) => outcome,
                    Some(Err(error)) => {
                        // Connection lost, auth refused: this subscription cannot continue, and the
                        // pipeline re-establishes the source when its own loop retries.
                        let unavailable = ConfigSourceError::Unavailable {
                            source_name: SOURCE_NAME,
                            source: Box::new(error),
                        };
                        let _ = send(&sender, &task_token, Err(unavailable)).await;
                        return;
                    }
                    None => {
                        let unavailable = ConfigSourceError::Unavailable {
                            source_name: SOURCE_NAME,
                            source: "the etcd watch stream ended".into(),
                        };
                        let _ = send(&sender, &task_token, Err(unavailable)).await;
                        return;
                    }
                };

                match outcome {
                    // The subscription is live: events follow.
                    WatchOutcome::Established => {}
                    // A batch of changes, each carrying its own revision and complete value.
                    WatchOutcome::Events { events } => {
                        for event in events {
                            let revision = match &event {
                                WatchEvent::Put { mod_revision, .. } => *mod_revision,
                                WatchEvent::Delete => {
                                    // The key is gone: the configuration source is unusable, and
                                    // reporting an empty document would overwrite the last known
                                    // good snapshot with nothing.
                                    let _ = send(&sender, &task_token, Err(Self::deleted())).await;
                                    return;
                                }
                            };
                            // No duplicate, no gap: anything at or before the revision in hand is
                            // history this stream already carried.
                            if matches!(revision.cmp(&last), Cmp::Less | Cmp::Equal) {
                                continue;
                            }
                            last = revision;
                            let WatchEvent::Put { value, .. } = event else {
                                continue;
                            };
                            let bytes = match checked(&value, max_bytes) {
                                Ok(bytes) => bytes,
                                Err(error) => {
                                    let _ = send(&sender, &task_token, Err(error)).await;
                                    return;
                                }
                            };
                            let version = ConfigVersion::new(Revision::comparable(revision), bytes);
                            // The fake source's rule for a consumer that fell behind: never block
                            // the producer, never buffer unbounded history. One resync replaces
                            // everything this stream had left, and the pipeline reloads the document.
                            match sender.try_send(Ok(ConfigEvent::Snapshot(version))) {
                                Ok(()) => {}
                                Err(_) => {
                                    let _ =
                                        send(&sender, &task_token, Ok(ConfigEvent::ResyncRequired))
                                            .await;
                                    return;
                                }
                            }
                        }
                    }
                    // History before `start_revision` is compacted away: the incremental stream
                    // cannot be trusted to be gapless, so the consumer reloads the whole document.
                    WatchOutcome::Canceled { .. } => {
                        let _ = send(&sender, &task_token, Ok(ConfigEvent::ResyncRequired)).await;
                        return;
                    }
                    // The server fragmented the events. Reassembly is not this adapter's job; a
                    // full reload is the honest answer.
                    WatchOutcome::Fragmented => {
                        let _ = send(&sender, &task_token, Ok(ConfigEvent::ResyncRequired)).await;
                        return;
                    }
                }
            }
        });

        Ok(Box::pin(WatchBridge { receiver, token }))
    }
}
