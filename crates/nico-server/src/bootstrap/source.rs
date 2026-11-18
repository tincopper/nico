//! The `config_source` section: where this process reads its document from, and how it refreshes it.
//!
//! The section is per-type by design: a local document has a path and file-watch triggers, an etcd
//! key has endpoints and a key name, and neither can describe the other — writing `path` into an
//! etcd source declaration is an unknown-field startup error, and so is the reverse. The validated
//! selection is the [`SourceSelection`]; it carries plain data so the construction site decides
//! which type it can build.
//!
//! It is also the whole of the *configuration acquisition* subsystem, which is why the pacing of a
//! reload (`debounce_ms`, `max_wait_ms`), its fallback (`cache_dir`, `allow_bootstrap_cache`) and
//! the shape bounds of the document it reads (`schema_bounds`) live here rather than at the top
//! level: none of them depends on the source type, and all of them are facts about *this*
//! subsystem. They keep their meaning and their rules exactly as they were; only their field paths
//! gained the `config_source.` prefix.
//!
//! Two rules govern the type dispatch:
//!
//! - **A validated bootstrap never represents a source this build cannot serve.** `etcd` is
//!   accepted only when compiled with the `etcd` feature; every other build refuses it at load
//!   with the list of types it does serve.
//! - **The endpoints pass through untouched.** Multi-endpoint failover is the etcd client's own
//!   behaviour beneath the [`crate::source seam`](nico_config::ConfigSource); the bootstrap bounds
//!   only the *shape* of each endpoint so a typo fails at startup with a field name.

use std::path::PathBuf;

use nico_config::WatchMode;
use nico_core::{Bound, SchemaBounds};
use serde::Deserialize;
use serde_norway::Value;

use super::BootstrapError;

/// Largest accepted `config_source.debounce_ms`.
const MAX_DEBOUNCE_MS: u64 = 10_000;

/// Largest accepted `config_source.max_wait_ms`.
const MAX_MAX_WAIT_MS: u64 = 60_000;

/// Largest accepted `config_source.schema_bounds.max_routes`, `max_upstreams`, `max_providers` and
/// `max_plugins`.
const MAX_COLLECTION_BOUND: usize = 1_000_000;

/// Largest accepted `config_source.schema_bounds.max_nodes`.
const MAX_NODES_BOUND: usize = 10_000_000;

/// Largest accepted `config_source.schema_bounds.max_depth`.
const MAX_DEPTH_BOUND: usize = 128;

/// The keys the reload subsystem owns inside `config_source`, in one place.
///
/// They are read from the section body and then removed from it, so each source type keeps a closed
/// shape: a grouped key cannot be mistaken for a misspelt source field, and a misspelt grouped key
/// cannot hide among the fields of a type body.
const RELOAD_KEYS: [&str; 5] = [
    "debounce_ms",
    "max_wait_ms",
    "cache_dir",
    "allow_bootstrap_cache",
    "schema_bounds",
];

/// The resolved `config_source` section: the authoritative selection plus the refresh behaviour the
/// reload pipeline reads.
///
/// Fields are plain data because this is the bootstrap's own view of the section; [`super::BootstrapConfig`]
/// is where they become a validated configuration, and its accessors are what the process reads.
#[derive(Debug)]
pub(super) struct SourceSection {
    /// The one authoritative source the section selects.
    pub(super) selection: SourceSelection,
    /// The maximum accepted configuration document size in bytes.
    pub(super) max_bytes: usize,
    /// How long a burst of changes is coalesced before publishing.
    pub(super) debounce_ms: u64,
    /// The longest a publish may be postponed while changes keep arriving.
    pub(super) max_wait_ms: u64,
    /// The directory the last-known-good cache lives in.
    pub(super) cache_dir: PathBuf,
    /// Whether a cached document may be served before the source has ever answered.
    pub(super) allow_bootstrap_cache: bool,
    /// The shape bounds every parsed document is held to.
    pub(super) schema_bounds: SchemaBounds,
}

/// The one authoritative source a bootstrap file selects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceSelection {
    /// A local configuration document, watched by file notifications and/or a signal.
    Local {
        /// The document path.
        path: PathBuf,
        /// How the local source detects changes.
        watch_mode: WatchMode,
    },
    /// One etcd key holding the complete configuration document, reached through any endpoint.
    #[cfg(feature = "etcd")]
    Etcd {
        /// The endpoints the client balances across.
        endpoints: Vec<String>,
        /// The key holding the document.
        key: String,
    },
}

/// The `config_source` body's types, for the unsupported-type error text.
fn supported_sources() -> &'static str {
    if cfg!(feature = "etcd") {
        "`local`, `etcd`"
    } else {
        "`local`"
    }
}

/// Reads the `config_source` body into its typed selection and the reload behaviour it carries.
///
/// The body is parsed in two steps because a tagged enum cannot also deny unknown fields: the
/// `type` value is read first, and only the matching schema is applied to the full body — so a
/// wrong field for the declared type is a startup error, never an ignored one. The reload keys are
/// read into their own half before that, and taken out of the body, because they belong to the
/// subsystem rather than to any one source type.
///
/// # Errors
/// - [`BootstrapError::Parse`] when the body does not match the schema of its own declared type.
/// - [`BootstrapError::UnsupportedSource`] when the type is not one this build serves.
/// - [`BootstrapError::InvalidField`] when the etcd endpoints or key are not usable shapes, when
///   the document size bound is zero, or when a reload knob is out of its accepted range.
pub(super) fn parse_source(value: Value) -> Result<SourceSection, BootstrapError> {
    let kind = serde_norway::from_value::<RawSourceKind>(value.clone())
        .map_err(|source| BootstrapError::Parse { source })?
        .kind;
    // Read the grouped half before the body loses the keys; both errors are `Parse`, so the order
    // only decides which field a doubly broken section reports.
    let reload = RawReload::read(&value)?;
    let body = without_reload_keys(value);

    let (selection, max_bytes) = match kind.as_str() {
        "local" => {
            let raw: RawLocalSource = serde_norway::from_value(body)
                .map_err(|source| BootstrapError::Parse { source })?;
            let watch_mode = parse_watch_mode(&raw.watch)?;
            (
                SourceSelection::Local {
                    path: raw.path,
                    watch_mode,
                },
                raw.max_bytes,
            )
        }
        #[cfg(feature = "etcd")]
        "etcd" => {
            let raw: RawEtcdSource = serde_norway::from_value(body)
                .map_err(|source| BootstrapError::Parse { source })?;
            validate_endpoints(&raw.endpoints)?;
            validate_key(&raw.key)?;
            (
                SourceSelection::Etcd {
                    endpoints: raw.endpoints,
                    key: raw.key,
                },
                raw.max_bytes,
            )
        }
        other => {
            return Err(BootstrapError::UnsupportedSource {
                found: other.to_owned(),
                serves: supported_sources(),
            });
        }
    };

    if max_bytes == 0 {
        return Err(BootstrapError::InvalidField {
            field: "config_source.max_bytes".into(),
            reason: "must be greater than zero".into(),
        });
    }
    let reload = reload.resolve()?;

    Ok(SourceSection {
        selection,
        max_bytes,
        debounce_ms: reload.debounce_ms,
        max_wait_ms: reload.max_wait_ms,
        cache_dir: reload.cache_dir,
        allow_bootstrap_cache: reload.allow_bootstrap_cache,
        schema_bounds: reload.schema_bounds,
    })
}

/// Takes the reload keys out of the section body, leaving the source type's own fields.
///
/// The value is returned unchanged when it is not a mapping: the type body's own parse is what
/// reports that, with the deserializer's wording instead of a sentence invented here.
fn without_reload_keys(value: Value) -> Value {
    let Value::Mapping(mut body) = value else {
        return value;
    };
    for key in RELOAD_KEYS {
        body.remove(key);
    }
    Value::Mapping(body)
}

/// The reload half of the `config_source` section exactly as written.
///
/// Every field is required except `schema_bounds`, exactly as when they were top-level: the path
/// change is not a change of semantics, and a knob a deployment must state is still stated.
/// Unknown keys are ignored here on purpose — the source type's own body is the closed shape, and
/// this struct is only the slice of it the reload subsystem owns.
#[derive(Debug, Deserialize)]
struct RawReload {
    debounce_ms: u64,
    max_wait_ms: u64,
    cache_dir: PathBuf,
    allow_bootstrap_cache: bool,
    schema_bounds: Option<RawSchemaBounds>,
}

/// The validated reload half.
struct Reload {
    debounce_ms: u64,
    max_wait_ms: u64,
    cache_dir: PathBuf,
    allow_bootstrap_cache: bool,
    schema_bounds: SchemaBounds,
}

impl RawReload {
    /// Reads the reload half out of a section body.
    ///
    /// # Errors
    /// Returns [`BootstrapError::Parse`] when a grouped key is missing or has the wrong type.
    fn read(value: &Value) -> Result<Self, BootstrapError> {
        serde_norway::from_value(value.clone()).map_err(|source| BootstrapError::Parse { source })
    }

    /// Applies the bounds an operator must stay within.
    ///
    /// # Errors
    /// Returns [`BootstrapError::InvalidField`] under `config_source.` when a window is above its
    /// ceiling, when the maximum wait is shorter than the debounce, or when a shape bound is
    /// outside its operator range.
    fn resolve(self) -> Result<Reload, BootstrapError> {
        if self.debounce_ms > MAX_DEBOUNCE_MS {
            return Err(BootstrapError::InvalidField {
                field: "config_source.debounce_ms".into(),
                reason: format!(
                    "must be in 0..={MAX_DEBOUNCE_MS} (found {})",
                    self.debounce_ms
                )
                .into(),
            });
        }
        if self.max_wait_ms < self.debounce_ms {
            return Err(BootstrapError::InvalidField {
                field: "config_source.max_wait_ms".into(),
                reason: "must be at least `debounce_ms`".into(),
            });
        }
        if self.max_wait_ms > MAX_MAX_WAIT_MS {
            return Err(BootstrapError::InvalidField {
                field: "config_source.max_wait_ms".into(),
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

        Ok(Reload {
            debounce_ms: self.debounce_ms,
            max_wait_ms: self.max_wait_ms,
            cache_dir: self.cache_dir,
            allow_bootstrap_cache: self.allow_bootstrap_cache,
            schema_bounds,
        })
    }
}

/// The optional `config_source.schema_bounds` block exactly as written.
///
/// Every field is optional, so the whole block or any single field can be omitted. A field that is
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
    ///
    /// # Errors
    /// Returns [`BootstrapError::InvalidField`] naming `config_source.schema_bounds.<bound>` when a
    /// written value is zero or above the limit an operator may write.
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
pub(super) const fn maximum(bound: Bound) -> usize {
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
        field: format!("config_source.schema_bounds.{}", bound.name()).into(),
        reason: format!("must be in 1..={} (found {value})", maximum(bound)).into(),
    }
}

/// The discriminator every `config_source` body carries, read before the type is known.
#[derive(Debug, Deserialize)]
struct RawSourceKind {
    #[serde(rename = "type")]
    kind: String,
}

/// The `local` body exactly as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLocalSource {
    #[serde(rename = "type")]
    _kind: String,
    path: PathBuf,
    max_bytes: usize,
    watch: String,
}

/// The `etcd` body exactly as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg(feature = "etcd")]
struct RawEtcdSource {
    #[serde(rename = "type")]
    _kind: String,
    endpoints: Vec<String>,
    key: String,
    max_bytes: usize,
}

/// Parses the local source's trigger modes, accepting the two triggers and their combination.
fn parse_watch_mode(value: &str) -> Result<WatchMode, BootstrapError> {
    match value {
        "notify" => Ok(WatchMode::Notify),
        "signal" => Ok(WatchMode::Signal),
        "notify+signal" | "signal+notify" => Ok(WatchMode::NotifyAndSignal),
        _ => Err(BootstrapError::InvalidField {
            field: "config_source.watch".into(),
            reason: "must be one of `notify`, `signal`, `notify+signal`".into(),
        }),
    }
}

/// Validates the endpoint list: at least one endpoint, each a usable shape.
#[cfg(feature = "etcd")]
fn validate_endpoints(endpoints: &[String]) -> Result<(), BootstrapError> {
    if endpoints.is_empty() {
        return Err(BootstrapError::InvalidField {
            field: "config_source.endpoints".into(),
            reason: "must list at least one endpoint".into(),
        });
    }
    for (index, endpoint) in endpoints.iter().enumerate() {
        validate_endpoint(endpoint, index)?;
    }
    Ok(())
}

/// Validates one endpoint's shape.
///
/// `etcd-client` accepts an `http://` endpoint as written, prefixes `http://` onto a bare
/// `host:port`, and this build links no TLS feature — an `https` endpoint can never be served, so
/// it is refused here with that reason instead of at connect. The full URI parse belongs to the
/// client; a shape-bounded endpoint that survives this check fails at connect only when the host
/// is genuinely wrong.
#[cfg(feature = "etcd")]
fn validate_endpoint(endpoint: &str, index: usize) -> Result<(), BootstrapError> {
    let field = format!("config_source.endpoints[{index}]");
    let invalid = |reason: String| BootstrapError::InvalidField {
        field: field.clone().into(),
        reason: reason.into(),
    };

    if endpoint.trim() != endpoint {
        return Err(invalid("must not contain surrounding whitespace".into()));
    }
    let authority = if let Some(rest) = endpoint.strip_prefix("http://") {
        rest
    } else if endpoint.starts_with("https://") {
        return Err(invalid(
            "is an https endpoint, but this build's etcd client has no TLS feature; serve an \
             http:// endpoint"
                .into(),
        ));
    } else {
        endpoint
    };
    // Everything after the first `/` is a path the etcd client would only serve the gRPC protocol
    // on; a declared path cannot be honoured, so the authority alone is what is validated.
    let authority = authority.split('/').next().unwrap_or("");
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };

    if host.is_empty() || (host.contains(':') && !host.starts_with('[')) {
        return Err(invalid(format!("has an unusable host `{host}`")));
    }
    if let Some(port) = port {
        let port: u16 = port
            .parse()
            .map_err(|_| invalid(format!("has an unusable port `{port}`; it must be numeric")))?;
        if port == 0 {
            return Err(invalid("has port 0; ports start at 1".into()));
        }
    }
    Ok(())
}

/// Validates the watched key: a non-empty, unpadded key name.
#[cfg(feature = "etcd")]
fn validate_key(key: &str) -> Result<(), BootstrapError> {
    if key.trim() != key || key.is_empty() {
        return Err(BootstrapError::InvalidField {
            field: "config_source.key".into(),
            reason: "must be a non-empty key name without surrounding whitespace".into(),
        });
    }
    Ok(())
}
