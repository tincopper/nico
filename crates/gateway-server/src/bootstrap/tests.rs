//! Unit tests for [`BootstrapConfig`](super::BootstrapConfig) and its raw shape.
//!
//! Kept beside the module rather than inline because the fixtures plus the range checks are
//! longer than the module they exercise, and the 500-line budget applies to the parent file.

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
            document("./resources.yaml", "notify", 100, 500).replace("type: local", "type: etcd");
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
        let bootstrap = load_document(&document("/etc/gateway/resources.yaml", "signal", 50, 90))
            .expect("the document is valid");
        let config = bootstrap.local_config();

        assert_eq!(config.path(), Path::new("/etc/gateway/resources.yaml"));
        assert_eq!(config.max_bytes(), 4096);
        assert_eq!(config.watch_mode(), WatchMode::Signal);
    }
}

mod schema_bounds {
    use gateway_core::Bound;

    use super::*;

    /// Bounds a test writes, paired with the value it expects to read back.
    const WRITTEN: [(Bound, usize); 5] = [
        (Bound::Routes, 8),
        (Bound::Upstreams, 7),
        (Bound::Providers, 6),
        (Bound::Nodes, 5),
        (Bound::Depth, 4),
    ];

    /// Appends a `schema_bounds` section to an otherwise complete bootstrap document.
    fn with_bounds(section: &str) -> String {
        format!(
            "{}schema_bounds:\n{section}",
            document("./resources.yaml", "notify", 100, 500)
        )
    }

    /// Loads a document whose `schema_bounds` section holds `section` and returns its parse limits.
    fn limits(section: &str) -> ParseLimits {
        load_document(&with_bounds(section))
            .expect("the fixture is a valid bootstrap document")
            .parse_limits()
    }

    /// Returns the section line that writes `value` for `bound`.
    fn line(bound: Bound, value: usize) -> String {
        format!("  {}: {value}\n", bound.name())
    }

    /// Asserts that `section` is rejected as an out-of-range bound, naming `bound`.
    fn expect_out_of_range(bound: Bound, section: &str) {
        match load_document(&with_bounds(section)) {
            Err(BootstrapError::InvalidField { field, reason }) => {
                assert_eq!(field, format!("schema_bounds.{}", bound.name()));
                assert!(
                    reason.contains(&format!("1..={}", maximum(bound))),
                    "the message must quote the enforced limit, got {reason:?}"
                );
            }
            other => panic!("expected InvalidField for {}, got {other:?}", bound.name()),
        }
    }

    #[test]
    fn an_omitted_section_uses_the_built_in_bounds() {
        let bootstrap = load_document(&document("./resources.yaml", "notify", 100, 500))
            .expect("the section is optional");

        assert_eq!(bootstrap.parse_limits().schema(), SchemaBounds::default());
        assert_eq!(bootstrap.parse_limits().max_bytes(), 4096);
    }

    #[test]
    fn an_omitted_field_keeps_its_built_in_bound() {
        let parsed = limits(&line(Bound::Routes, 8));
        let defaults = SchemaBounds::default();

        assert_eq!(parsed.schema().get(Bound::Routes), 8);
        for (bound, _) in WRITTEN {
            if bound != Bound::Routes {
                assert_eq!(
                    parsed.schema().get(bound),
                    defaults.get(bound),
                    "{} changed although it was omitted",
                    bound.name()
                );
            }
        }
    }

    #[test]
    fn every_written_field_reaches_the_parse_limits() {
        let section = WRITTEN.map(|(bound, value)| line(bound, value)).concat();
        let parsed = limits(&section);

        for (bound, value) in WRITTEN {
            assert_eq!(
                parsed.schema().get(bound),
                value,
                "{} was not applied",
                bound.name()
            );
        }
    }

    #[test]
    fn an_unknown_bound_is_rejected() {
        let error = load_document(&with_bounds("  max_routez: 8\n"))
            .expect_err("a misspelt bound must not be absorbed by the default");

        assert!(
            matches!(error, BootstrapError::Parse { .. }),
            "expected Parse, got {error:?}"
        );
    }

    #[test]
    fn a_zero_bound_is_rejected_for_every_field() {
        for (bound, _) in WRITTEN {
            expect_out_of_range(bound, &line(bound, 0));
        }
    }

    #[test]
    fn a_bound_above_the_operator_maximum_is_rejected_for_every_field() {
        for (bound, _) in WRITTEN {
            expect_out_of_range(bound, &line(bound, maximum(bound) + 1));
        }
    }
}
