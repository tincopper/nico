//! Bootstrap configuration: the local file that says how the gateway process runs and where it
//! reads its configuration.
//!
//! This file is read before any configuration source exists, so it is never served *by* a
//! configuration source. A remote control plane must not be able to rewrite where the process reads
//! its configuration from, which local directory it writes its cache to, which sockets it binds, or
//! which certificate it presents on them.
//!
//! It is organised the way nginx's own configuration is: one section per layer, and the section's
//! presence is the fact it states.
//!
//! - `runtime` — the process: worker threads and the two shutdown windows. Optional; absent means
//!   every default.
//! - `http` — the protocol and the inbound side: one `listeners[]` entry per socket, each a
//!   plaintext listener or, when it carries a `tls` block, a TLS listener. **Required**: a gateway
//!   that binds nothing serves nothing, so this is not a default the build can invent.
//! - `proxy` — the outbound side: the roots an *upstream* handshake is verified against. Optional;
//!   absent means the platform certificate store.
//! - `config_source` — where the document comes from, and how it is watched, de-bounced, cached and
//!   bounded.
//! - `metrics` — the self-reporting surface. Optional; absent means off.
//!
//! It contains no routes: routing is runtime configuration, and it arrives through the source.
//!
//! [`BootstrapConfig`] can only be built by [`BootstrapConfig::load`], so every instance is already
//! validated: the source type is known, the watch mode is parsed, the listeners are legal and
//! pairwise distinct, and the timing, trust and shape bounds are sane.

use std::{
    borrow::Cow,
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

use nico_core::{ParseLimits, SchemaBounds};
use nico_http::{HttpSettings, ServerSettings, TlsPolicy};
use serde::Deserialize;
use serde_norway::Value;

mod http;
mod inbound_tls;
mod metrics;
mod proxy;
mod quota_store;
mod runtime;
mod source;
mod tls;

pub use metrics::MetricsSettings;
use quota_store::parse_quota_store;
pub use quota_store::{QuotaEtcdAuth, QuotaEtcdTls, QuotaStoreSelection, UnavailablePolicy};
#[cfg(feature = "etcd")]
pub(crate) use source::MAX_TLS_MATERIAL_BYTES;
pub use source::SourceSelection;
use source::parse_source;
#[cfg(feature = "etcd")]
pub use source::{EtcdAuth, EtcdTls};

/// A validated bootstrap configuration.
///
/// Fields are private and there is no public constructor besides [`BootstrapConfig::load`], because
/// an unvalidated bootstrap would let a deployment select a source type this build cannot serve, or
/// bind a listener whose certificate was never read.
#[derive(Debug, Clone, PartialEq)]
pub struct BootstrapConfig {
    source: SourceSelection,
    source_max_bytes: usize,
    debounce_ms: u64,
    max_wait_ms: u64,
    cache_dir: PathBuf,
    allow_bootstrap_cache: bool,
    schema_bounds: SchemaBounds,
    quota_store: QuotaStoreSelection,
    runtime: ServerSettings,
    http: HttpSettings,
    metrics: Option<MetricsSettings>,
}

impl BootstrapConfig {
    /// Reads and validates a bootstrap file.
    ///
    /// # Errors
    /// - [`BootstrapError::Read`] when the file cannot be read.
    /// - [`BootstrapError::Parse`] when it is not YAML, has unknown or missing fields, or has a
    ///   field of the wrong type.
    /// - [`BootstrapError::UnsupportedSource`] when `config_source.type` is not a type this build
    ///   serves.
    /// - [`BootstrapError::InvalidField`] when a field is syntactically fine but out of range, for
    ///   example an unknown watch mode, a `max_wait_ms` below `debounce_ms`, an unusable listener
    ///   address, or a certificate and key that are not a pair.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, BootstrapError> {
        let bytes = fs::read(path.as_ref()).map_err(|source| BootstrapError::Read { source })?;
        let raw: RawBootstrap =
            serde_norway::from_slice(&bytes).map_err(|source| BootstrapError::Parse { source })?;
        raw.validate()
    }

    /// Returns the authoritative source the file selects.
    ///
    /// The selection is plain data; the construction site decides which type this build can build.
    #[must_use]
    pub const fn source(&self) -> &SourceSelection {
        &self.source
    }

    /// Returns the maximum accepted configuration document size in bytes.
    #[must_use]
    pub const fn source_max_bytes(&self) -> usize {
        self.source_max_bytes
    }

    /// Returns how long a burst of changes is coalesced before publishing.
    #[must_use]
    pub const fn debounce_ms(&self) -> u64 {
        self.debounce_ms
    }

    /// Returns the longest a publish may be postponed while changes keep arriving.
    #[must_use]
    pub const fn max_wait_ms(&self) -> u64 {
        self.max_wait_ms
    }

    /// Returns the directory the last-known-good cache lives in.
    #[must_use]
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Returns whether a cached document may be served before the source has ever answered.
    #[must_use]
    pub const fn allow_bootstrap_cache(&self) -> bool {
        self.allow_bootstrap_cache
    }

    /// Returns the process-level runtime settings: worker threads and the two shutdown windows.
    ///
    /// They are validated here and nowhere else, so a listener or a drain window cannot disagree
    /// between the binary that assembles the server and the service that runs it. An absent
    /// `runtime` section resolves to [`ServerSettings::new`], so this is never `None`: a process
    /// always has a thread count and a drain window, it just may not have written them down.
    #[must_use]
    pub const fn runtime(&self) -> &ServerSettings {
        &self.runtime
    }

    /// Returns the process-level settings of the HTTP data plane: its listeners, `h2c` and the
    /// outbound trust roots.
    ///
    /// They are validated here and nowhere else, so a listener cannot be assembled from values the
    /// bootstrap never accepted. The `http` section is required, so this is never `None`.
    #[must_use]
    pub const fn http(&self) -> &HttpSettings {
        &self.http
    }

    /// Returns the settings of the self-reporting listener, or `None` when the surface is off.
    ///
    /// The section is optional, and an omitted section is the default: no second listener, no
    /// recorder, and no behaviour change. A present section is validated here and nowhere else.
    #[must_use]
    pub const fn metrics(&self) -> Option<&MetricsSettings> {
        self.metrics.as_ref()
    }

    /// Returns where the token budgets live.
    ///
    /// An omitted `quota_store` section is [`QuotaStoreSelection::Local`], the standing
    /// in-process table; a written one names the shared backend and the failure policy a
    /// multi-replica deployment has chosen.
    #[must_use]
    pub const fn quota_store(&self) -> &QuotaStoreSelection {
        &self.quota_store
    }

    /// Returns the failure policy the selected store is served with.
    ///
    /// Local has no backend to be unavailable, so its policy is fail-closed by construction —
    /// and unreachable, because the in-process store cannot fail. A shared backend's policy is
    /// the one the section required.
    #[must_use]
    pub const fn quota_policy(&self) -> UnavailablePolicy {
        match &self.quota_store {
            QuotaStoreSelection::Local => UnavailablePolicy::FailClosed,
            QuotaStoreSelection::Etcd { on_unavailable, .. } => *on_unavailable,
            QuotaStoreSelection::Redis { on_unavailable, .. } => *on_unavailable,
        }
    }

    /// Builds the parse limits the pipeline applies to every document.
    ///
    /// The byte bound is `config_source.max_bytes`; the shape bounds are the
    /// `config_source.schema_bounds` block with every omitted field filled from
    /// [`SchemaBounds::default`]. This is the pipeline's only source of parse limits, so the file
    /// and the built-in defaults cannot diverge.
    #[must_use]
    pub fn parse_limits(&self) -> ParseLimits {
        ParseLimits::new(self.source_max_bytes).with_schema(self.schema_bounds)
    }

    /// Returns the debounce window as a duration.
    #[must_use]
    pub const fn debounce(&self) -> Duration {
        Duration::from_millis(self.debounce_ms)
    }

    /// Returns the maximum wait as a duration.
    #[must_use]
    pub const fn max_wait(&self) -> Duration {
        Duration::from_millis(self.max_wait_ms)
    }
}

/// A bootstrap file that could not be read, parsed or accepted.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BootstrapError {
    /// The bootstrap file could not be read.
    #[error("bootstrap configuration could not be read")]
    Read {
        /// Underlying file-system error.
        #[source]
        source: io::Error,
    },
    /// The bootstrap file is not valid YAML, or does not match the schema.
    #[error("bootstrap configuration is not a valid gateway bootstrap document")]
    Parse {
        /// Parser or schema error, including unknown and missing fields.
        #[source]
        source: serde_norway::Error,
    },
    /// The bootstrap file selects a configuration source this build does not implement.
    #[error("unsupported configuration source type: {found}; this build serves {serves}")]
    UnsupportedSource {
        /// The rejected `config_source.type` value.
        found: String,
        /// The types this build does serve.
        serves: &'static str,
    },
    /// A field is syntactically valid but outside the range this build accepts.
    ///
    /// Both parts borrow for a fixed rule and own only when they quote a limit, so a message can
    /// never drift from the constant the check applied.
    #[error("bootstrap field `{field}` is invalid: {reason}")]
    InvalidField {
        /// Dotted field path, for example `config_source.watch`.
        field: Cow<'static, str>,
        /// Why the value was rejected. Never contains credentials.
        reason: Cow<'static, str>,
    },
}

// Compiled-in proof that a bootstrap error can cross `.await` and `tokio::spawn` boundaries.
const _: fn() = || {
    const fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<BootstrapError>();
};

/// The bootstrap file exactly as written, before any interpretation.
///
/// Keeping this separate from [`BootstrapConfig`] is what makes the validated type unable to hold an
/// unparsed watch mode, an unsupported source type, or a listener whose certificate was never read.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBootstrap {
    // The body is typed per `config_source.type`, which serde's tagged enums cannot express
    // together with unknown-field rejection — see `source::parse_source`.
    config_source: Value,
    // Omitted means "every default", so the process-level knobs stay optional.
    runtime: Option<runtime::RawRuntime>,
    // Required: a gateway with no listener serves nothing, and a default port would silently open a
    // socket no operator asked for.
    http: http::RawHttp,
    // Present-but-empty is a configuration error, not "no section": see `proxy::section`.
    #[serde(default, deserialize_with = "proxy::section")]
    proxy: Option<proxy::RawProxy>,
    metrics: Option<metrics::RawMetrics>,
    // Optional: omitted means the standing in-process budget table. A written section is parsed
    // in two steps like `config_source`, so it is kept as a raw value here.
    quota_store: Option<Value>,
}

impl RawBootstrap {
    fn validate(self) -> Result<BootstrapConfig, BootstrapError> {
        // The `config_source` section owns the document's provenance and the pacing of its reload.
        let source = parse_source(self.config_source)?;

        // The outbound trust roots are resolved before the listeners because they are an input to
        // the settings the `http` section produces: a configured bundle is a field of
        // `HttpSettings`, not a knob some hook reads on its own. An omitted `proxy` is the platform
        // store, exactly as the top-level `tls` section used to be.
        let trust = match &self.proxy {
            Some(raw) => raw.resolve()?,
            None => TlsPolicy::from_bootstrap(None),
        };

        // An omitted `runtime` section and an empty one behave identically, so both go through
        // `resolve`: the defaults are applied and validated on the same path.
        let runtime = match &self.runtime {
            Some(raw) => raw.resolve()?,
            None => ServerSettings::new(),
        };

        let http = self.http.resolve(trust)?;

        // The metrics section is resolved against every address the data plane binds, so it can
        // only collide when both are written; an omitted section stays `None`.
        let metrics = match &self.metrics {
            Some(raw) => Some(raw.resolve(&http.listener_addresses())?),
            None => None,
        };

        // The quota store is parsed like `config_source`: per-type, closed, and never guessed.
        // Omitted is the in-process table, so the default deployment parses nothing here.
        let quota_store = match &self.quota_store {
            Some(value) => parse_quota_store(value.clone())?,
            None => QuotaStoreSelection::Local,
        };

        Ok(BootstrapConfig {
            source: source.selection,
            source_max_bytes: source.max_bytes,
            debounce_ms: source.debounce_ms,
            max_wait_ms: source.max_wait_ms,
            cache_dir: source.cache_dir,
            allow_bootstrap_cache: source.allow_bootstrap_cache,
            schema_bounds: source.schema_bounds,
            quota_store,
            runtime,
            http,
            metrics,
        })
    }
}

#[cfg(test)]
mod tests;
