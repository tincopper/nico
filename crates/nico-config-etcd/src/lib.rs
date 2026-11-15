//! The etcd configuration source.
//!
//! This crate implements nico's [`ConfigSource`](nico_config::ConfigSource) contract for a single
//! etcd key that holds one complete configuration document, and it does so **without speaking to
//! etcd**. Every etcd interaction enters through the [`client`] seam: a small trait modelling the
//! two operations the source needs (`get` and `watch`) and the typed shape of etcd's watch
//! protocol. Tests drive that seam with a scripted client, which is why every semantic this source
//! implements is assertable without a real server.
//!
//! The real etcd client is deliberately **not** here yet. etcd's client is a tonic/gRPC stack —
//! a second network stack beside pingora's — so the dependency is deferred until an environment
//! exists to wire and verify it. When it arrives, it implements the same seam and none of this
//! crate's logic changes.
//!
//! # Semantics
//!
//! - **One key is one complete document.** A `put` event is a new snapshot; a `delete` means the
//!   source is no longer usable and is reported as unavailable, never as an empty document.
//! - **Revisions are etcd's.** `load` reads the key's `mod_revision`; `watch` subscribes from
//!   `load`'s revision plus one, inclusive. Events at or below the seen revision are dropped, so
//!   the stream neither repeats nor gaps.
//! - **Compaction and fragmentation are resyncs.** A watch cancelled with a `compact_revision`, or
//!   one whose events the server fragmented, cannot continue incrementally: the consumer reloads
//!   the whole document.
//! - **Connection failures are unavailable.** Reconnecting, endpoint failover and TLS belong to
//!   the real client implementation, beneath the seam; the adapter folds every such failure into
//!   the contract's "cannot be read" answer and lets the pipeline decide when to retry.

#![deny(missing_docs)]

mod client;
#[cfg(test)]
mod mock;
mod source;

#[cfg(test)]
mod tests;

#[cfg(feature = "client")]
pub use client::real::GrpcEtcdClient;
pub use client::{EtcdClient, EtcdError, KvEntry, WatchEvent, WatchOutcome, WatchStream};
pub use source::EtcdConfigSource;
