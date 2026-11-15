//! The seam where a real etcd client will plug in.
//!
//! Two operations are all the adapter needs, and this trait is the only place the wire appears. A
//! real implementation (the `etcd-client` crate's watch and range calls) arrives when an
//! environment exists to verify it; tests drive the same seam with a scripted client, which is why
//! the adapter's semantics are testable without a server.

use bytes::Bytes;

/// One stored key-value pair as the adapter needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KvEntry {
    /// The document bytes, exactly as stored.
    pub(crate) value: Bytes,
    /// etcd's `mod_revision`: the revision this value was written at.
    pub(crate) mod_revision: u64,
}

/// One step of an etcd watch, typed rather than a bag of gRPC fields.
///
/// The variants are the five outcomes the adapter must distinguish; anything the real client
/// reports maps onto one of them, and the mapping is that implementation's job.
#[derive(Debug)]
pub enum WatchOutcome {
    /// The watch was created and is streaming.
    Established,
    /// A batch of events. etcd delivers events in batches with a shared header revision; the
    /// per-event `mod_revision` is what the adapter orders by.
    Events {
        /// The events in delivery order.
        events: Vec<WatchEvent>,
    },
    /// The watch was cancelled because the requested history is compacted. The consumer cannot
    /// continue incrementally and must reload the whole document.
    Canceled {
        /// The revision the store was compacted to, when the cancellation names one.
        compact_revision: Option<u64>,
    },
    /// The server fragmented this watch's events. The adapter does not reassemble fragments; a
    /// fragmented watch is answered with a full reload.
    Fragmented,
}

/// One change to the watched key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    /// The key was written: a new complete document at `mod_revision`.
    Put {
        /// The document bytes.
        value: Bytes,
        /// The revision this value was written at.
        mod_revision: u64,
    },
    /// The key was deleted. The configuration source is no longer usable.
    Delete,
}

/// The watch stream a client returns: a channel of typed protocol outcomes.
///
/// A channel rather than a `Stream` so neither the adapter nor a mock needs a stream combinator:
/// a real implementation forwards its gRPC stream into the channel from one forwarding task, and a
/// test fills it before the watch is established.
pub type WatchStream = tokio::sync::mpsc::Receiver<Result<WatchOutcome, EtcdError>>;

/// Why a client operation failed.
///
/// The variants are the shapes the adapter must tell apart; everything else about an etcd error
/// (its gRPC status, its endpoint) stays inside the real implementation.
#[derive(Debug, thiserror::Error)]
pub enum EtcdError {
    /// The server could not be reached, or the operation timed out.
    #[error("the etcd server could not be reached: {message}")]
    Connect {
        /// What failed, in the implementation's own words.
        message: String,
    },
    /// The server refused the operation: bad credentials, permission denied.
    #[error("etcd refused the operation: {message}")]
    Auth {
        /// What was refused, in the implementation's own words.
        message: String,
    },
}

/// The client seam: read one key once, and watch it from a revision.
///
/// `watch`'s `start_revision` is etcd's own: the stream begins with events **at** that revision,
/// inclusive. The adapter subscribes from `load`'s revision plus one, so nothing between the load
/// and the subscription is lost.
#[async_trait::async_trait]
pub trait EtcdClient: Send + Sync {
    /// Reads the watched key, returning `None` when it does not exist.
    ///
    /// # Errors
    /// Returns [`EtcdError`] when the server cannot be reached or refuses the read.
    async fn get(&self, key: &str) -> Result<Option<KvEntry>, EtcdError>;

    /// Watches the key from `start_revision`, inclusive, and streams the protocol's outcomes.
    ///
    /// # Errors
    /// Returns [`EtcdError`] when the subscription cannot be established.
    async fn watch(&self, key: &str, start_revision: u64) -> Result<WatchStream, EtcdError>;
}

#[cfg(all(test, feature = "client"))]
pub(crate) mod mock_server;
#[cfg(feature = "client")]
mod proto;
#[cfg(feature = "client")]
pub(crate) mod real;
