//! Configuration sources for the gateway.
//!
//! This crate owns one narrow contract — [`ConfigSource`] — and the implementations that satisfy it
//! today: a [local file source](local) and a [scripted fake](fake) for tests. The
//! [last-known-good cache](cache) lives here too, because it is written from the same document bytes
//! the pipeline publishes.
//!
//! # Contract in one paragraph
//!
//! A source `load`s one complete document with a source-scoped revision, and `watch`es for changes.
//! A watch emits only complete snapshots or a resync request — there is no incremental patch,
//! because a half-applied document is never a valid configuration. Output is bounded: a consumer
//! that falls behind is told to resync rather than handed unlimited history. Dropping the stream
//! cancels the subscription, leaving no task, watcher or connection behind. A failure is reported as
//! a [`ConfigSourceError`], never as an empty document, so a missing file cannot silently become an
//! empty route table.
//!
//! The pipeline — debouncing, digest de-duplication, validation, compilation, publication — is not
//! here. It lives in `nico-server`, which is also the only crate that knows which concrete source
//! a process runs.
//!
//! # What a source must not do
//!
//! - Compute the document digest. The pipeline calls [`ConfigDigest::of`] once, so two adapters can
//!   never disagree about whether two documents are the same.
//! - Decide when to reload, or retry a failed reload.
//! - Expose protocol types (an etcd key, a Nacos data id, a `notify` event) in a public signature.
//!   An adapter absorbs its protocol's quirks — inclusive resume revisions, fragmented responses,
//!   deleted keys, failed authentication, reconnects — and reports only the contract's vocabulary.
//!
//! # Examples
//!
//! Loading a document through the trait, independent of the adapter:
//!
//! ```
//! use std::sync::Arc;
//!
//! use nico_config::{ConfigSource, FakeConfigSource};
//!
//! let source: Arc<dyn ConfigSource> =
//!     Arc::new(FakeConfigSource::new("demo").push_load(Ok(FakeConfigSource::version_at(1, b"doc"))));
//! let runtime = tokio::runtime::Builder::new_current_thread()
//!     .build()
//!     .expect("runtime builds");
//!
//! let version = runtime.block_on(source.load()).expect("scripted load succeeds");
//! assert_eq!(version.revision().seq(), 1);
//! ```
#![deny(missing_docs)]

pub mod cache;
pub mod fake;
pub mod file;
pub mod local;
pub mod source;
mod stream;

pub use cache::{CacheError, CacheMeta, CacheStore, CachedSnapshot};
pub use fake::FakeConfigSource;
pub use file::{ReadError, read_bounded};
pub use local::{LocalConfig, LocalFileSource, WatchMode};
pub use nico_core::version::{ConfigDigest, Revision, RevisionOrdering};
pub use source::{ConfigEvent, ConfigSource, ConfigSourceError, ConfigVersion, ConfigWatch};
