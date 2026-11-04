//! Bootstrap configuration: the local file that says where the gateway reads its configuration.
//!
//! This file is read before any configuration source exists, so it is never served *by* a
//! configuration source. A remote control plane must not be able to rewrite where the process reads
//! its configuration from, or which local directory it writes its cache to.
//!
//! The file selects one authoritative source, bounds how long a debounce may postpone a publish, and
//! states whether a cached document may be served before the source has ever answered. It contains
//! no routes: routing is runtime configuration, and it arrives through the source.
//!
//! [`BootstrapConfig`] can only be built by [`BootstrapConfig::load`], so every instance is already
//! validated: the source type is known, the watch mode is parsed, and the timing bounds are sane.

use std::{
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

use gateway_config::{LocalConfig, WatchMode};
use serde::Deserialize;

/// The only accepted `config_source.type`.
const SUPPORTED_SOURCE: &str = "local";

/// Largest accepted `debounce_ms`.
const MAX_DEBOUNCE_MS: u64 = 10_000;

/// Largest accepted `max_wait_ms`.
const MAX_MAX_WAIT_MS: u64 = 60_000;

/// A validated bootstrap configuration.
///
/// Fields are private and there is no public constructor besides [`BootstrapConfig::load`], because
/// an unvalidated bootstrap would let a deployment select a source type this build cannot serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapConfig {
    source_path: PathBuf,
    source_max_bytes: usize,
    watch_mode: WatchMode,
    debounce_ms: u64,
    max_wait_ms: u64,
    cache_dir: PathBuf,
    allow_bootstrap_cache: bool,
}

impl BootstrapConfig {
    /// Reads and validates a bootstrap file.
    ///
    /// # Errors
    /// - [`BootstrapError::Read`] when the file cannot be read.
    /// - [`BootstrapError::Parse`] when it is not YAML, has unknown or missing fields, or has a
    ///   field of the wrong type.
    /// - [`BootstrapError::UnsupportedSource`] when `config_source.type` is not `local`.
    /// - [`BootstrapError::InvalidField`] when a field is syntactically fine but out of range, for
    ///   example an unknown watch mode or a `max_wait_ms` below `debounce_ms`.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, BootstrapError> {
        let bytes = fs::read(path.as_ref()).map_err(|source| BootstrapError::Read { source })?;
        let raw: RawBootstrap =
            serde_norway::from_slice(&bytes).map_err(|source| BootstrapError::Parse { source })?;
        raw.validate()
    }

    /// Returns the path of the configuration document.
    #[must_use]
    pub fn source_path(&self) -> &Path {
        &self.source_path
    }

    /// Returns the maximum accepted configuration document size in bytes.
    #[must_use]
    pub const fn source_max_bytes(&self) -> usize {
        self.source_max_bytes
    }

    /// Returns how the source detects changes.
    #[must_use]
    pub const fn watch_mode(&self) -> WatchMode {
        self.watch_mode
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

    /// Builds the local source configuration described by this file.
    ///
    /// The source type is not returned: `local` is the only type [`Self::load`] accepts, so a caller
    /// cannot accidentally match on a field that is already known.
    #[must_use]
    pub fn local_config(&self) -> LocalConfig {
        LocalConfig::new(self.source_path.clone())
            .with_max_bytes(self.source_max_bytes)
            .with_watch_mode(self.watch_mode)
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
    #[error("unsupported configuration source type: {found}")]
    UnsupportedSource {
        /// The rejected `config_source.type` value.
        found: String,
    },
    /// A field is syntactically valid but outside the range this build accepts.
    #[error("bootstrap field `{field}` is invalid: {reason}")]
    InvalidField {
        /// Dotted field path, for example `config_source.watch`.
        field: &'static str,
        /// Why the value was rejected. Never contains credentials.
        reason: &'static str,
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
    config_source: RawSource,
    debounce_ms: u64,
    max_wait_ms: u64,
    cache_dir: PathBuf,
    allow_bootstrap_cache: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSource {
    #[serde(rename = "type")]
    kind: String,
    path: PathBuf,
    max_bytes: usize,
    watch: String,
}

impl RawBootstrap {
    fn validate(self) -> Result<BootstrapConfig, BootstrapError> {
        if self.config_source.kind != SUPPORTED_SOURCE {
            return Err(BootstrapError::UnsupportedSource {
                found: self.config_source.kind,
            });
        }

        let watch_mode = parse_watch_mode(&self.config_source.watch)?;

        if self.config_source.max_bytes == 0 {
            return Err(BootstrapError::InvalidField {
                field: "config_source.max_bytes",
                reason: "must be greater than zero",
            });
        }
        if self.debounce_ms > MAX_DEBOUNCE_MS {
            return Err(BootstrapError::InvalidField {
                field: "debounce_ms",
                reason: "must not exceed 10000",
            });
        }
        if self.max_wait_ms < self.debounce_ms {
            return Err(BootstrapError::InvalidField {
                field: "max_wait_ms",
                reason: "must be at least debounce_ms",
            });
        }
        if self.max_wait_ms > MAX_MAX_WAIT_MS {
            return Err(BootstrapError::InvalidField {
                field: "max_wait_ms",
                reason: "must not exceed 60000",
            });
        }

        Ok(BootstrapConfig {
            source_path: self.config_source.path,
            source_max_bytes: self.config_source.max_bytes,
            watch_mode,
            debounce_ms: self.debounce_ms,
            max_wait_ms: self.max_wait_ms,
            cache_dir: self.cache_dir,
            allow_bootstrap_cache: self.allow_bootstrap_cache,
        })
    }
}

/// Parses the `watch` field, accepting only the two trigger sources and their combination.
fn parse_watch_mode(value: &str) -> Result<WatchMode, BootstrapError> {
    match value {
        "notify" => Ok(WatchMode::Notify),
        "signal" => Ok(WatchMode::Signal),
        "notify+signal" | "signal+notify" => Ok(WatchMode::NotifyAndSignal),
        _ => Err(BootstrapError::InvalidField {
            field: "config_source.watch",
            reason: "must be one of `notify`, `signal`, `notify+signal`",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(path: &str, watch: &str, debounce_ms: u64, max_wait_ms: u64) -> String {
        format!(
            "\
config_source:
  type: local
  path: {path}
  max_bytes: 4096
  watch: {watch}
debounce_ms: {debounce_ms}
max_wait_ms: {max_wait_ms}
cache_dir: ./.tmp/cache
allow_bootstrap_cache: false
"
        )
    }

    /// Writes `document` to a temporary file and loads it.
    fn load_document(document: &str) -> Result<BootstrapConfig, BootstrapError> {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let path = directory.path().join("bootstrap.yaml");
        std::fs::write(&path, document).expect("fixture write succeeds");
        BootstrapConfig::load(&path)
    }

    mod load {
        use super::*;

        #[test]
        fn load_accepts_the_shipped_bootstrap_file() {
            let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/bootstrap.yaml");
            let bootstrap = BootstrapConfig::load(&path).expect("the shipped bootstrap is valid");

            assert_eq!(bootstrap.watch_mode(), WatchMode::NotifyAndSignal);
            assert_eq!(bootstrap.debounce_ms(), 200);
            assert_eq!(bootstrap.max_wait_ms(), 1_000);
            assert_eq!(bootstrap.source_max_bytes(), 1_048_576);
            assert!(!bootstrap.allow_bootstrap_cache());
            assert_eq!(bootstrap.debounce(), Duration::from_millis(200));
            assert_eq!(bootstrap.max_wait(), Duration::from_millis(1_000));
        }

        #[test]
        fn load_reports_a_missing_file_as_read() {
            let directory = tempfile::TempDir::new().expect("temporary directory");
            let error = BootstrapConfig::load(directory.path().join("absent.yaml"))
                .expect_err("the file does not exist");
            match error {
                BootstrapError::Read { source } => {
                    assert_eq!(source.kind(), io::ErrorKind::NotFound);
                }
                other => panic!("expected Read, got {other:?}"),
            }
        }

        #[test]
        fn load_rejects_unknown_fields() {
            let mut text = document("./resources.yaml", "notify", 100, 500);
            text.push_str("routes: []\n");
            let error = load_document(&text).expect_err("unknown field");
            assert!(
                matches!(error, BootstrapError::Parse { .. }),
                "expected Parse, got {error:?}"
            );
        }

        #[test]
        fn load_rejects_an_unknown_source_type() {
            let text = document("./resources.yaml", "notify", 100, 500)
                .replace("type: local", "type: etcd");
            match load_document(&text).expect_err("etcd is not implemented yet") {
                BootstrapError::UnsupportedSource { found } => assert_eq!(found, "etcd"),
                other => panic!("expected UnsupportedSource, got {other:?}"),
            }
        }

        #[test]
        fn load_rejects_an_unknown_watch_mode() {
            let text = document("./resources.yaml", "poll", 100, 500);
            match load_document(&text).expect_err("`poll` is not a trigger source") {
                BootstrapError::InvalidField { field, .. } => {
                    assert_eq!(field, "config_source.watch");
                }
                other => panic!("expected InvalidField, got {other:?}"),
            }
        }

        #[test]
        fn load_accepts_every_documented_watch_mode() {
            for (text, expected) in [
                ("notify", WatchMode::Notify),
                ("signal", WatchMode::Signal),
                ("notify+signal", WatchMode::NotifyAndSignal),
            ] {
                let bootstrap = load_document(&document("./resources.yaml", text, 100, 500))
                    .expect("the watch mode is documented");
                assert_eq!(bootstrap.watch_mode(), expected);
            }
        }

        #[test]
        fn load_rejects_max_wait_below_the_debounce_window() {
            let text = document("./resources.yaml", "notify", 500, 200);
            match load_document(&text).expect_err("max_wait must not be shorter than debounce") {
                BootstrapError::InvalidField { field, .. } => assert_eq!(field, "max_wait_ms"),
                other => panic!("expected InvalidField, got {other:?}"),
            }
        }

        #[test]
        fn load_rejects_a_debounce_window_above_ten_seconds() {
            let text = document("./resources.yaml", "notify", 10_001, 20_000);
            match load_document(&text).expect_err("debounce is bounded") {
                BootstrapError::InvalidField { field, .. } => assert_eq!(field, "debounce_ms"),
                other => panic!("expected InvalidField, got {other:?}"),
            }
        }

        #[test]
        fn load_rejects_a_max_wait_above_sixty_seconds() {
            let text = document("./resources.yaml", "notify", 100, 60_001);
            match load_document(&text).expect_err("max_wait is bounded") {
                BootstrapError::InvalidField { field, .. } => assert_eq!(field, "max_wait_ms"),
                other => panic!("expected InvalidField, got {other:?}"),
            }
        }

        #[test]
        fn load_rejects_a_zero_document_size_bound() {
            let text = document("./resources.yaml", "notify", 100, 500)
                .replace("max_bytes: 4096", "max_bytes: 0");
            match load_document(&text).expect_err("a zero bound accepts no document") {
                BootstrapError::InvalidField { field, .. } => {
                    assert_eq!(field, "config_source.max_bytes");
                }
                other => panic!("expected InvalidField, got {other:?}"),
            }
        }
    }

    mod local_config {
        use super::*;

        #[test]
        fn local_config_carries_the_path_the_bound_and_the_watch_mode() {
            let bootstrap =
                load_document(&document("/etc/gateway/resources.yaml", "signal", 50, 90))
                    .expect("the document is valid");
            let config = bootstrap.local_config();

            assert_eq!(config.path(), Path::new("/etc/gateway/resources.yaml"));
            assert_eq!(config.max_bytes(), 4096);
            assert_eq!(config.watch_mode(), WatchMode::Signal);
        }
    }
}
