//! Protocol-agnostic kernel shared by every gateway data plane.
//!
//! The crate holds only what more than one data plane genuinely needs:
//!
//! - [`config`] — the versioned document model, generation, revision and digest types.
//! - [`snapshot`] — the immutable runtime snapshot and its publication handle.
//! - [`status`] — the operator-visible state of the configuration pipeline.
//! - [`error`] — error classification that never names a wire format.
//!
//! Nothing here depends on a protocol crate, a config-center client or a file-system watcher; wire
//! mapping happens at the data-plane edge.
#![deny(missing_docs)]

pub mod config;
pub mod error;
pub mod snapshot;
pub mod status;

pub use config::{
    API_VERSION, ConfigDigest, Document, DocumentError, Generation, GenerationOverflow, Limits,
    ParseLimits, Revision, RevisionOrdering,
};
pub use error::ErrorCategory;
pub use snapshot::{RuntimeSnapshot, SnapshotHandle};
pub use status::ConfigStatus;
