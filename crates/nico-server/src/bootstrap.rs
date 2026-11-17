//! Bootstrap configuration: the local file that says where the gateway reads its configuration.
//!
//! This file is read before any configuration source exists, so it is never served *by* a
//! configuration source. A remote control plane must not be able to rewrite where the process reads
//! its configuration from, or which local directory it writes its cache to.
//!
//! The file selects one authoritative source, bounds how long a debounce may postpone a publish, and
//! states whether a cached document may be served before the source has ever answered. It also
//! carries the optional parse-time shape bounds, so an operator can tighten or widen them without a
//! rebuild — but only from this local file, never from the configuration source whose size those
//! bounds are meant to contain.
//!
//! It contains no routes: routing is runtime configuration, and it arrives through the source.
//!
//! [`BootstrapConfig`] can only be built by [`BootstrapConfig::load`], so every instance is already
//! validated: the source type is known, the watch mode is parsed, and the timing and shape bounds are
//! sane.

use std::{
    borrow::Cow,
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

use nico_core::{Bound, ParseLimits, SchemaBounds};
use nico_http::{HttpSettings, TlsPolicy};
use serde::Deserialize;
use serde_norway::Value;

mod http;
mod metrics;
mod source;
mod tls;

pub use metrics::MetricsSettings;
pub use source::SourceSelection;
use source::parse_source;

/// Largest accepted `debounce_ms`.
const MAX_DEBOUNCE_MS: u64 = 10_000;

/// Largest accepted `max_wait_ms`.
const MAX_MAX_WAIT_MS: u64 = 60_000;

/// Largest accepted `schema_bounds.max_routes`, `max_upstreams`, `max_providers` and `max_plugins`.
const MAX_COLLECTION_BOUND: usize = 1_000_000;

/// Largest accepted `schema_bounds.max_nodes`.
const MAX_NODES_BOUND: usize = 10_000_000;

/// Largest accepted `schema_bounds.max_depth`.
const MAX_DEPTH_BOUND: usize = 128;

/// A validated bootstrap configuration.
///
/// Fields are private and there is no public constructor besides [`BootstrapConfig::load`], because
/// an unvalidated bootstrap would let a deployment select a source type this build cannot serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapConfig {
    source: SourceSelection,
    source_max_bytes: usize,
    debounce_ms: u64,
    max_wait_ms: u64,
    cache_dir: PathBuf,
    allow_bootstrap_cache: bool,
    schema_bounds: SchemaBounds,
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
    ///   example an unknown watch mode or a `max_wait_ms` below `debounce_ms`.
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

    /// Returns the process-level settings of the HTTP data plane.
    ///
    /// They are validated here and nowhere else, so a listener or a drain window cannot disagree
    /// between the binary that assembles the server and the service that runs it.
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

    /// Builds the parse limits the pipeline applies to every document.
    ///
    /// The byte bound is `config_source.max_bytes`; the shape bounds are the `schema_bounds` section
    /// with every omitted field filled from [`SchemaBounds::default`]. This is the pipeline's only
    /// source of parse limits, so the file and the built-in defaults cannot diverge.
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
/// unparsed watch mode or an unsupported source type.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBootstrap {
    // The body is typed per `config_source.type`, which serde's tagged enums cannot express
    // together with unknown-field rejection — see `source::parse_source`.
    config_source: Value,
    schema_bounds: Option<RawSchemaBounds>,
    http: Option<http::RawHttp>,
    metrics: Option<metrics::RawMetrics>,
    // Present-but-empty is a configuration error, not "no section": see `tls::section`.
    #[serde(default, deserialize_with = "tls::section")]
    tls: Option<tls::RawTls>,
    debounce_ms: u64,
    max_wait_ms: u64,
    cache_dir: PathBuf,
    allow_bootstrap_cache: bool,
}

/// The optional `schema_bounds` section exactly as written.
///
/// Every field is optional, so the whole section or any single field can be omitted. A field that is
/// absent keeps its [`SchemaBounds::default`] value; a field that is *present* must be accepted as
/// written, so a typo or a `0` fails at startup instead of being absorbed by the default. The
/// built-in values are never restated here: an omitted field is simply not applied.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSchemaBounds {
    max_routes: Option<usize>,
    max_upstreams: Option<usize>,
    max_providers: Option<usize>,
    max_plugins: Option<usize>,
    max_endpoints: Option<usize>,
    max_pool_members: Option<usize>,
    max_nodes: Option<usize>,
    max_depth: Option<usize>,
}

impl RawSchemaBounds {
    /// Applies every written bound over the defaults, rejecting one outside its operator range.
    fn resolve(&self) -> Result<SchemaBounds, BootstrapError> {
        let mut bounds = SchemaBounds::default();
        for (bound, written) in [
            (Bound::Routes, self.max_routes),
            (Bound::Upstreams, self.max_upstreams),
            (Bound::Providers, self.max_providers),
            (Bound::Plugins, self.max_plugins),
            (Bound::Endpoints, self.max_endpoints),
            (Bound::PoolMembers, self.max_pool_members),
            (Bound::Nodes, self.max_nodes),
            (Bound::Depth, self.max_depth),
        ] {
            let Some(value) = written else { continue };
            if value > maximum(bound) {
                return Err(out_of_range(bound, value));
            }
            bounds = bounds
                .with(bound, value)
                // The upper bound was checked above, so `with` can only reject a `0` here.
                .map_err(|_error| out_of_range(bound, value))?;
        }
        Ok(bounds)
    }
}

/// Returns the largest value an operator may write for `bound`.
///
/// This is a bootstrap policy, not a property of [`SchemaBounds`]: the parser accepts any non-zero
/// bound, while a hand-written file is bounded so a typo cannot ask for a million routes. The match
/// is exhaustive on purpose — a new bound must state its own limit instead of inheriting one.
const fn maximum(bound: Bound) -> usize {
    match bound {
        Bound::Routes
        | Bound::Upstreams
        | Bound::Providers
        | Bound::Plugins
        | Bound::Endpoints
        | Bound::PoolMembers => MAX_COLLECTION_BOUND,
        Bound::Nodes => MAX_NODES_BOUND,
        Bound::Depth => MAX_DEPTH_BOUND,
    }
}

/// Builds the error for a bound an operator may not write.
fn out_of_range(bound: Bound, value: usize) -> BootstrapError {
    BootstrapError::InvalidField {
        field: format!("schema_bounds.{}", bound.name()).into(),
        reason: format!("must be in 1..={} (found {value})", maximum(bound)).into(),
    }
}

impl RawBootstrap {
    fn validate(self) -> Result<BootstrapConfig, BootstrapError> {
        let (source, source_max_bytes) = parse_source(self.config_source)?;

        if source_max_bytes == 0 {
            return Err(BootstrapError::InvalidField {
                field: "config_source.max_bytes".into(),
                reason: "must be greater than zero".into(),
            });
        }
        if self.debounce_ms > MAX_DEBOUNCE_MS {
            return Err(BootstrapError::InvalidField {
                field: "debounce_ms".into(),
                reason: format!(
                    "must be in 0..={MAX_DEBOUNCE_MS} (found {})",
                    self.debounce_ms
                )
                .into(),
            });
        }
        if self.max_wait_ms < self.debounce_ms {
            return Err(BootstrapError::InvalidField {
                field: "max_wait_ms".into(),
                reason: "must be at least debounce_ms".into(),
            });
        }
        if self.max_wait_ms > MAX_MAX_WAIT_MS {
            return Err(BootstrapError::InvalidField {
                field: "max_wait_ms".into(),
                reason: format!(
                    "must be in 0..={MAX_MAX_WAIT_MS} (found {})",
                    self.max_wait_ms
                )
                .into(),
            });
        }

        let schema_bounds = match &self.schema_bounds {
            Some(raw) => raw.resolve()?,
            None => SchemaBounds::default(),
        };

        // The trust roots are resolved first because they are an input to the settings the `http`
        // section produces: a configured bundle is a field of `HttpSettings`, not a knob some hook
        // reads on its own. An omitted `tls` is the platform store, exactly as before the section
        // existed.
        let tls = match &self.tls {
            Some(raw) => raw.resolve()?,
            None => TlsPolicy::from_bootstrap(None),
        };

        // An omitted section and an empty section must behave identically, so both go through
        // `resolve`: the defaults are applied and validated on the same path.
        let http = match &self.http {
            Some(raw) => raw.resolve(tls)?,
            None => http::RawHttp::default().resolve(tls)?,
        };

        // The metrics section is resolved against the data plane's own address, so it can only
        // collide when both are written; an omitted section stays `None`.
        let metrics = match &self.metrics {
            Some(raw) => Some(raw.resolve(http.listen())?),
            None => None,
        };

        Ok(BootstrapConfig {
            source,
            source_max_bytes,
            debounce_ms: self.debounce_ms,
            max_wait_ms: self.max_wait_ms,
            cache_dir: self.cache_dir,
            allow_bootstrap_cache: self.allow_bootstrap_cache,
            schema_bounds,
            http,
            metrics,
        })
    }
}

#[cfg(test)]
mod tests;
