//! The `config_source` section's shared half: the reload knobs and the document's shape bounds.
//!
//! Both used to be top-level facts and moved into the section in one step. Their rules are
//! unchanged — only the field path gained the `config_source.` prefix — so these tests assert the
//! new paths *and* that the old top-level keys are now unknown fields rather than silently ignored
//! ones.
//!
//! Kept in its own file because the fixture matrix for eight bounds plus five knobs is longer than
//! the section it exercises, and the 500-line budget applies to the parent file.

use nico_core::Bound;

use super::*;

/// Bounds a test writes, paired with the value it expects to read back.
const WRITTEN: [(Bound, usize); 7] = [
    (Bound::Routes, 8),
    (Bound::Upstreams, 7),
    (Bound::Providers, 6),
    (Bound::Plugins, 3),
    (Bound::Endpoints, 2),
    (Bound::Nodes, 5),
    (Bound::Depth, 4),
];

/// Appends top-level lines to an otherwise complete bootstrap document.
fn with_extra(lines: &str) -> String {
    format!(
        "{}{lines}",
        document("./resources.yaml", "notify", 100, 500)
    )
}

/// Appends a `schema_bounds` block to the `config_source` section of a complete document.
fn with_bounds(section: &str) -> String {
    format!(
        "{}  schema_bounds:\n{}http:\n  listeners:\n    - listen: 127.0.0.1:8080\n",
        config_source("./resources.yaml", "notify", 100, 500),
        section
    )
}

/// Loads a document whose `config_source.schema_bounds` block holds `section`.
fn limits(section: &str) -> ParseLimits {
    load_document(&with_bounds(section))
        .expect("the fixture is a valid bootstrap document")
        .parse_limits()
}

/// Returns the block line that writes `value` for `bound`.
fn line(bound: Bound, value: usize) -> String {
    format!("    {}: {value}\n", bound.name())
}

/// Asserts that `section` is rejected as an out-of-range bound, naming `bound`.
fn expect_out_of_range(bound: Bound, section: &str) {
    match load_document(&with_bounds(section)) {
        Err(BootstrapError::InvalidField { field, reason }) => {
            assert_eq!(
                field,
                format!("config_source.schema_bounds.{}", bound.name())
            );
            assert!(
                reason.contains(&format!("1..={}", source::maximum(bound))),
                "the message must quote the enforced limit, got {reason:?}"
            );
        }
        other => panic!("expected InvalidField for {}, got {other:?}", bound.name()),
    }
}

#[test]
fn the_grouped_knobs_reach_their_accessors() {
    let text = document("./resources.yaml", "notify", 250, 750)
        .replace("cache_dir: ./.tmp/cache", "cache_dir: /var/cache/nico")
        .replace(
            "allow_bootstrap_cache: false",
            "allow_bootstrap_cache: true",
        );
    let bootstrap = load_document(&text).expect("the document is valid");

    assert_eq!(bootstrap.debounce_ms(), 250);
    assert_eq!(bootstrap.max_wait_ms(), 750);
    assert_eq!(bootstrap.cache_dir(), Path::new("/var/cache/nico"));
    assert!(bootstrap.allow_bootstrap_cache());
    assert_eq!(
        bootstrap.parse_limits().schema(),
        SchemaBounds::default(),
        "an omitted schema_bounds block is the built-in shape, not an empty one"
    );
}

#[test]
fn the_old_top_level_knobs_are_unknown_fields() {
    for line in [
        "debounce_ms: 100\n",
        "max_wait_ms: 500\n",
        "cache_dir: ./.tmp/other\n",
        "allow_bootstrap_cache: true\n",
        "schema_bounds:\n  max_routes: 8\n",
    ] {
        let error = load_document(&with_extra(line)).expect_err("the top level holds no knobs");

        assert!(
            matches!(error, BootstrapError::Parse { .. }),
            "`{line}` must be an unknown top-level field, got {error:?}"
        );
    }
}

#[test]
fn a_missing_reload_knob_is_a_parse_error() {
    let text = document("./resources.yaml", "notify", 100, 500).replace("  debounce_ms: 100\n", "");
    let error = load_document(&text).expect_err("a reload knob is not defaulted");

    assert!(
        matches!(error, BootstrapError::Parse { .. }),
        "the path changed, not the requirement, got {error:?}"
    );
}

#[test]
fn an_omitted_block_uses_the_built_in_bounds() {
    let bootstrap = load_document(&document("./resources.yaml", "notify", 100, 500))
        .expect("the block is optional");

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
    let error = load_document(&with_bounds("    max_routez: 8\n"))
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
        expect_out_of_range(bound, &line(bound, source::maximum(bound) + 1));
    }
}
