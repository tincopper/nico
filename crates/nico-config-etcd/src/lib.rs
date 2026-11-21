//! The etcd configuration source.
//!
//! This crate implements nico's [`ConfigSource`](nico_config::ConfigSource) contract for a single
//! etcd key that holds one complete configuration document, and it does so **without speaking to
//! etcd**. Every etcd interaction enters through the [`EtcdClient`] seam: a small trait modelling the
//! two operations the source needs (`get` and `watch`) and the typed shape of etcd's watch
//! protocol. Tests drive that seam with a scripted client, which is why every semantic this source
//! implements is assertable without a real server.
//!
//! The real etcd client lives behind the default-on `client` feature: a tonic/gRPC stack — a second
//! network stack beside pingora's — so the product binary keeps it gated (`nico-server`'s `etcd`
//! feature is off by default) and the gating is verified per release. It implements the same seam,
//! and none of this crate's adapter logic depends on which client drives it. What it is configured
//! with is [`EtcdConnectOptions`] — TLS material and credentials as bytes and values, never paths:
//! where a process keeps a certificate, and whether it re-checks it before connecting, belongs to the
//! caller. No trust root is inherited from the platform store, and no credential is presented unless
//! one was given.
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

#[cfg(feature = "mock-server")]
pub use client::mock_server::{MockServer, MockServerBuilder, StoreState};
#[cfg(feature = "client")]
pub use client::options::{EtcdConnectOptions, EtcdCredentials, EtcdTlsOptions};
#[cfg(feature = "client")]
pub use client::real::GrpcEtcdClient;
pub use client::{EtcdClient, EtcdError, KvEntry, WatchEvent, WatchOutcome, WatchStream};
pub use source::EtcdConfigSource;
