//! The `config_source` section: where this process reads its document from, and how it refreshes it.
//!
//! The section is per-type by design: a local document has a path and file-watch triggers, an etcd
//! key has endpoints, a key name and the transport those endpoints are reached over, and neither can
//! describe the other — writing `path` into an etcd source declaration is an unknown-field startup
//! error, and so is the reverse. The validated selection is the [`SourceSelection`]; it carries plain
//! data so the construction site decides which type it can build.
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
//!
//! # Modules
//!
//! - `bounds` — the document's shape bounds (`config_source.schema_bounds`), and the operator ranges
//!   a written one must stay within.
//! - `etcd` — the `etcd` body: its endpoints, its `tls` and `auth` blocks, and the scheme matrix
//!   that keeps a transport from being guessed.

use std::path::PathBuf;

use nico_config::WatchMode;
use nico_core::SchemaBounds;
use serde::Deserialize;
use serde_norway::Value;

use super::BootstrapError;

mod bounds;
#[cfg(feature = "etcd")]
mod etcd;

/// The operator ranges a written bound must stay within.
///
/// Re-exported for the tests, which pin every `schema_bounds` refusal against the same function the
/// parser applies: a hand-copied limit in a test would pass while the parser drifted.
#[cfg(test)]
pub(super) use bounds::maximum;
/// Re-exported for `source_build`, which re-applies the same ceiling on its second read of the
/// material, so the two reads cannot disagree on size.
#[cfg(feature = "etcd")]
pub(crate) use etcd::MAX_TLS_MATERIAL_BYTES;
#[cfg(feature = "etcd")]
pub use etcd::{EtcdAuth, EtcdTls};

/// Largest accepted `config_source.debounce_ms`.
const MAX_DEBOUNCE_MS: u64 = 10_000;

/// Largest accepted `config_source.max_wait_ms`.
const MAX_MAX_WAIT_MS: u64 = 60_000;

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
        /// The transport every endpoint is reached over, when the section configures TLS.
        tls: Option<EtcdTls>,
        /// The credentials the client authenticates with, when the section configures them.
        auth: Option<EtcdAuth>,
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
/// - [`BootstrapError::InvalidField`] when the etcd endpoints, key, transport or credentials are not
///   usable, when the document size bound is zero, or when a reload knob is out of its accepted
///   range.
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
            let raw: etcd::RawEtcdSource = serde_norway::from_value(body)
                .map_err(|source| BootstrapError::Parse { source })?;
            etcd::resolve(raw)?
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
    schema_bounds: Option<bounds::RawSchemaBounds>,
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
