//! Unit tests for [`BootstrapConfig`](super::BootstrapConfig) and its raw shape.
//!
//! Kept beside the module rather than inline because the fixtures plus the range checks are
//! longer than the module they exercise, and the 500-line budget applies to the parent file.

use nico_config::WatchMode;

use super::*;

/// The `config_source` section every fixture shares, with the values a caller varies.
///
/// The reload knobs are written inside the section, which is where they live now: they are facts
/// about the configuration-acquisition subsystem, not about the process as a whole.
fn config_source(path: &str, watch: &str, debounce_ms: u64, max_wait_ms: u64) -> String {
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

/// A complete bootstrap document: [`config_source`] plus one plaintext listener.
fn document(path: &str, watch: &str, debounce_ms: u64, max_wait_ms: u64) -> String {
    format!(
        "{}http:\n  listeners:\n    - listen: 127.0.0.1:8080\n",
        config_source(path, watch, debounce_ms, max_wait_ms)
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

        let SourceSelection::Local { watch_mode, .. } = bootstrap.source() else {
            panic!("the shipped bootstrap selects the local source");
        };
        assert_eq!(*watch_mode, WatchMode::NotifyAndSignal);
        assert_eq!(bootstrap.debounce_ms(), 200);
        assert_eq!(bootstrap.max_wait_ms(), 1_000);
        assert_eq!(bootstrap.source_max_bytes(), 1_048_576);
        assert!(!bootstrap.allow_bootstrap_cache());
        assert_eq!(bootstrap.debounce(), Duration::from_millis(200));
        assert_eq!(bootstrap.max_wait(), Duration::from_millis(1_000));
        assert_eq!(
            bootstrap.parse_limits(),
            ParseLimits::new(1_048_576),
            "the shipped file must document the built-in bounds"
        );
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
        let text =
            document("./resources.yaml", "notify", 100, 500).replace("type: local", "type: nacos");
        match load_document(&text).expect_err("nacos is not a source type this build serves") {
            BootstrapError::UnsupportedSource { found, serves } => {
                assert_eq!(found, "nacos");
                assert!(
                    serves.contains("`local`"),
                    "the error must list the types this build serves, got {serves}"
                );
            }
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
            let SourceSelection::Local { watch_mode, .. } = bootstrap.source() else {
                panic!("the document selects the local source");
            };
            assert_eq!(*watch_mode, expected);
        }
    }

    #[test]
    fn load_rejects_max_wait_below_the_debounce_window() {
        let text = document("./resources.yaml", "notify", 500, 200);
        match load_document(&text).expect_err("max_wait must not be shorter than debounce") {
            BootstrapError::InvalidField { field, .. } => {
                assert_eq!(field, "config_source.max_wait_ms");
            }
            other => panic!("expected InvalidField, got {other:?}"),
        }
    }

    #[test]
    fn load_rejects_a_debounce_window_above_ten_seconds() {
        let text = document("./resources.yaml", "notify", 10_001, 20_000);
        match load_document(&text).expect_err("debounce is bounded") {
            BootstrapError::InvalidField { field, .. } => {
                assert_eq!(field, "config_source.debounce_ms");
            }
            other => panic!("expected InvalidField, got {other:?}"),
        }
    }

    #[test]
    fn load_rejects_a_max_wait_above_sixty_seconds() {
        let text = document("./resources.yaml", "notify", 100, 60_001);
        match load_document(&text).expect_err("max_wait is bounded") {
            BootstrapError::InvalidField { field, .. } => {
                assert_eq!(field, "config_source.max_wait_ms");
            }
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
    fn the_local_selection_carries_the_path_the_bound_and_the_watch_mode() {
        let bootstrap = load_document(&document("/etc/gateway/resources.yaml", "signal", 50, 90))
            .expect("the document is valid");
        let SourceSelection::Local { path, watch_mode } = bootstrap.source() else {
            panic!("the document selects the local source");
        };

        assert_eq!(path, &PathBuf::from("/etc/gateway/resources.yaml"));
        assert_eq!(*watch_mode, WatchMode::Signal);
        assert_eq!(bootstrap.source_max_bytes(), 4096);
    }
}

mod http {
    use nico_http::Listener;

    use super::*;

    /// A document whose `http` section holds `section`.
    fn with_http(section: &str) -> String {
        format!(
            "{}http:\n{section}",
            config_source("./resources.yaml", "notify", 100, 500)
        )
    }

    /// Loads a document whose `http` section holds `section`.
    fn launched(section: &str) -> Result<BootstrapConfig, BootstrapError> {
        load_document(&with_http(section))
    }

    /// Loads a document whose `http` section holds `section` and expects it to be rejected.
    fn rejection(section: &str) -> BootstrapError {
        launched(section)
            .err()
            .unwrap_or_else(|| panic!("`{section}` must be rejected"))
    }

    /// A plaintext listener on `127.0.0.1:9000`, the address most tests here use.
    const PLAINTEXT: &str = "  listeners:\n    - listen: 127.0.0.1:9000\n";

    #[test]
    fn an_absent_section_is_a_parse_error() {
        let error = load_document(&config_source("./resources.yaml", "notify", 100, 500))
            .expect_err("`http` is required");

        assert!(
            matches!(error, BootstrapError::Parse { .. }),
            "a gateway with no listener is not a default, got {error:?}"
        );
    }

    #[test]
    fn a_section_without_listeners_names_no_listener() {
        for section in ["", "  h2c: false\n", "  listeners: []\n"] {
            let error = rejection(section);

            assert!(
                matches!(
                    &error,
                    BootstrapError::InvalidField { field, .. } if field == "http.listeners"
                ),
                "expected an invalid http.listeners for `{section}`, got {error:?}"
            );
        }
    }

    #[test]
    fn one_plaintext_listener_reaches_the_settings() {
        let bootstrap = launched(PLAINTEXT).expect("the fixture is a valid listener");

        assert_eq!(
            bootstrap.http().listeners(),
            &[Listener::Plain {
                listen: "127.0.0.1:9000".to_owned(),
            }]
        );
        assert_eq!(bootstrap.http().listener_addresses(), ["127.0.0.1:9000"]);
        assert_eq!(bootstrap.http().plain_addresses(), ["127.0.0.1:9000"]);
        assert!(!bootstrap.http().h2c());
    }

    #[test]
    fn the_shipped_section_is_the_documented_listener() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/bootstrap.yaml");
        let bootstrap = BootstrapConfig::load(&path).expect("the shipped bootstrap is valid");

        assert_eq!(
            bootstrap.http().listener_addresses(),
            ["0.0.0.0:8080"],
            "the shipped file documents one plaintext listener instead of inventing policy"
        );
        assert!(
            matches!(bootstrap.http().listeners(), [Listener::Plain { .. }]),
            "and it terminates no TLS"
        );
        assert!(!bootstrap.http().h2c());
    }

    #[test]
    fn h2c_is_false_unless_it_is_explicitly_enabled() {
        assert!(!launched(PLAINTEXT).expect("valid").http().h2c());
        assert!(
            !launched(&format!("  h2c: false\n{PLAINTEXT}"))
                .expect("valid")
                .http()
                .h2c()
        );
        assert!(
            launched(&format!("  h2c: true\n{PLAINTEXT}"))
                .expect("h2c applies to a plaintext listener")
                .http()
                .h2c()
        );
    }

    #[test]
    fn an_unparsable_listen_address_is_reported_against_the_entry() {
        let error = rejection("  listeners:\n    - listen: nowhere\n");

        assert!(
            matches!(
                &error,
                BootstrapError::InvalidField { field, .. } if field == "http.listeners[0].listen"
            ),
            "expected an invalid http.listeners[0].listen, got {error:?}"
        );
    }

    #[test]
    fn a_zero_port_is_reported_against_the_entry() {
        let error = rejection("  listeners:\n    - listen: 0.0.0.0:0\n");

        assert!(
            matches!(
                &error,
                BootstrapError::InvalidField { field, .. } if field == "http.listeners[0].listen"
            ),
            "expected an invalid http.listeners[0].listen, got {error:?}"
        );
    }

    #[test]
    fn a_repeated_address_is_rejected() {
        let error = rejection(concat!(
            "  listeners:\n",
            "    - listen: 127.0.0.1:9000\n",
            "    - listen: 127.0.0.1:9000\n",
        ));

        match error {
            BootstrapError::InvalidField { field, reason } => {
                assert_eq!(field, "http.listeners");
                assert!(
                    reason.contains("must not repeat"),
                    "the reason states what the second entry did: {reason}"
                );
            }
            other => panic!("expected InvalidField, got {other:?}"),
        }
    }

    #[test]
    fn the_moved_process_knobs_are_unknown_fields() {
        for line in [
            "  threads: 2\n",
            "  grace_period_ms: 1000\n",
            "  graceful_shutdown_ms: 60000\n",
        ] {
            let error = rejection(&format!("{line}{PLAINTEXT}"));

            assert!(
                matches!(error, BootstrapError::Parse { .. }),
                "`{line}` belongs to `runtime` now, got {error:?}"
            );
        }
    }

    #[test]
    fn an_unknown_http_field_is_rejected_rather_than_ignored() {
        let error = rejection(&format!(
            "  h2c: true\n  listen: 127.0.0.1:9000\n{PLAINTEXT}"
        ));

        assert!(
            matches!(error, BootstrapError::Parse { .. }),
            "expected Parse, got {error:?}"
        );
    }
}

#[cfg(feature = "etcd")]
mod etcd_source;

mod config_source_section;
mod http_tls;
mod quota_store_section;
mod runtime;
mod tls;
