//! Protocol-agnostic kernel shared by every gateway data plane.
//!
//! The crate holds only what more than one data plane genuinely needs:
//!
//! - [`config`] — the versioned document model and its parse-time bounds.
//! - [`version`] — generation, revision and digest identifiers for published snapshots.
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
pub mod version;

pub use config::{
    API_VERSION, BodyValue, Bound, BoundsError, CircuitBreaker, DEFAULT_CONTENT_TYPE,
    DEFAULT_COOLDOWN_MS, DEFAULT_ENDPOINT_NAME, DEFAULT_ENDPOINT_WEIGHT, DEFAULT_FAIL_TIMEOUT_MS,
    DEFAULT_FAILURE_THRESHOLD, DEFAULT_MAX_FAILS, DEFAULT_MAX_RESPONSE_BYTES, Document,
    DocumentError, Ejection, Endpoint, ErrorBody, ErrorResponse, HostError, MAX_ATTEMPTS_PER_ROUTE,
    MAX_FALLBACK_MEMBERS, MAX_HOST_BYTES, ParseLimits, PluginSpec, Pool, PoolMember, PoolStrategy,
    Provider, ProxyHeaderValue, ProxySetHeader, RequestBounds, Route, RouteMode, RouteTarget,
    SchemaBounds, Scheme, Strategy, TokenBudget, Upstream, UpstreamPool, UpstreamPoolMember,
    attempt_ceiling, host_key, hosts_match, is_json_content_type, is_secret_ref, validate_host,
};
pub use error::ErrorCategory;
pub use snapshot::{RuntimeSnapshot, SnapshotHandle};
pub use status::ConfigStatus;
pub use version::{ConfigDigest, Generation, GenerationOverflow, Revision, RevisionOrdering};
