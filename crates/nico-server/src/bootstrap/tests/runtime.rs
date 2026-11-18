//! The optional `runtime` section: worker threads and the two shutdown windows.
//!
//! Kept in its own file for the reason its parent states: the fixture matrix for three knobs is
//! longer than the module they exercise, and the 500-line budget applies to the parent file.
//!
//! The rules are the ones the data plane owns, so these tests assert the *paths* as much as the
//! values: a knob that used to live under `http` must now be read from `runtime`, and the old path
//! must be an unknown field rather than an ignored one.

use super::*;

/// Appends a `runtime` section to an otherwise complete bootstrap document.
fn with_runtime(section: &str) -> String {
    format!(
        "{}runtime:\n{section}",
        document("./resources.yaml", "notify", 100, 500)
    )
}

/// Loads a document whose `runtime` section holds `section` and returns the resolved settings.
fn settings(section: &str) -> ServerSettings {
    load_document(&with_runtime(section))
        .expect("the fixture is a valid bootstrap document")
        .runtime()
        .clone()
}

/// Loads a document whose `runtime` section holds `section` and expects it to be rejected.
fn rejection(section: &str) -> BootstrapError {
    load_document(&with_runtime(section))
        .err()
        .unwrap_or_else(|| panic!("`{section}` must be rejected"))
}

#[test]
fn an_omitted_section_takes_every_default() {
    let bootstrap = load_document(&document("./resources.yaml", "notify", 100, 500))
        .expect("the document is valid");

    assert_eq!(
        bootstrap.runtime(),
        &ServerSettings::new(),
        "an omitted section must be the same value as its defaults"
    );
    assert_eq!(bootstrap.runtime(), &settings(""));
}

#[test]
fn the_shipped_section_is_the_documented_default() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/bootstrap.yaml");
    let bootstrap = BootstrapConfig::load(&path).expect("the shipped bootstrap is valid");

    assert_eq!(
        bootstrap.runtime(),
        &ServerSettings::new(),
        "the shipped file documents the defaults instead of inventing policy"
    );
    assert_eq!(bootstrap.runtime().threads(), 1);
    assert_eq!(bootstrap.runtime().grace_period_ms(), 5_000);
    assert_eq!(bootstrap.runtime().graceful_shutdown_ms(), 30_000);
}

#[test]
fn every_written_field_reaches_the_settings() {
    let settings = settings(concat!(
        "  threads: 8\n",
        "  grace_period_ms: 1000\n",
        "  graceful_shutdown_ms: 60000\n",
    ));

    assert_eq!(settings.threads(), 8);
    assert_eq!(settings.grace_period_ms(), 1_000);
    assert_eq!(settings.graceful_shutdown_ms(), 60_000);
}

#[test]
fn an_omitted_field_keeps_its_default_within_a_written_section() {
    let settings = settings("  threads: 2\n");

    assert_eq!(settings.threads(), 2);
    assert_eq!(settings.grace_period_ms(), 5_000);
    assert_eq!(settings.graceful_shutdown_ms(), 30_000);
}

#[test]
fn a_zero_thread_count_is_rejected_at_startup() {
    let error = rejection("  threads: 0\n");

    assert!(
        matches!(
            &error,
            BootstrapError::InvalidField { field, .. } if field == "runtime.threads"
        ),
        "expected an invalid runtime.threads, got {error:?}"
    );
}

#[test]
fn too_many_threads_are_rejected_at_startup() {
    let error = rejection("  threads: 257\n");

    assert!(
        matches!(
            &error,
            BootstrapError::InvalidField { field, .. } if field == "runtime.threads"
        ),
        "expected an invalid runtime.threads, got {error:?}"
    );
}

#[test]
fn a_zero_grace_period_is_rejected_at_startup() {
    let error = rejection("  grace_period_ms: 0\n");

    assert!(
        matches!(
            &error,
            BootstrapError::InvalidField { field, .. } if field == "runtime.grace_period_ms"
        ),
        "expected an invalid runtime.grace_period_ms, got {error:?}"
    );
}

#[test]
fn a_grace_period_above_the_maximum_is_rejected_at_startup() {
    let error = rejection("  grace_period_ms: 301000\n");

    assert!(
        matches!(
            &error,
            BootstrapError::InvalidField { field, .. } if field == "runtime.grace_period_ms"
        ),
        "expected an invalid runtime.grace_period_ms, got {error:?}"
    );
}

#[test]
fn a_zero_shutdown_window_is_rejected_at_startup() {
    let error = rejection("  graceful_shutdown_ms: 0\n");

    assert!(
        matches!(
            &error,
            BootstrapError::InvalidField { field, .. } if field == "runtime.graceful_shutdown_ms"
        ),
        "expected an invalid runtime.graceful_shutdown_ms, got {error:?}"
    );
}

#[test]
fn a_shutdown_window_shorter_than_the_grace_period_is_rejected_at_startup() {
    let error = rejection(concat!(
        "  grace_period_ms: 10000\n",
        "  graceful_shutdown_ms: 5000\n",
    ));

    assert!(
        matches!(
            &error,
            BootstrapError::InvalidField { field, .. } if field == "runtime.graceful_shutdown_ms"
        ),
        "expected an invalid runtime.graceful_shutdown_ms, got {error:?}"
    );
}

#[test]
fn an_unknown_runtime_field_is_rejected_rather_than_ignored() {
    let error = rejection("  thread: 2\n");

    assert!(
        matches!(error, BootstrapError::Parse { .. }),
        "a misspelt knob must not silently keep the default, got {error:?}"
    );
}
