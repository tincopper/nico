//! The self-reporting surface's dependency promises, checked against the manifests themselves.
//!
//! The reason `nico-metrics` exists as its own crate is a dependency fact, not a taste: the
//! Prometheus exporter must stay out of the data plane's tree, and the capability layer must stay
//! free of any network stack. Both are one line in a `Cargo.toml` away from being untrue, and a
//! review cannot be relied on to catch a line that looks like every other line. So they are
//! asserted here, from the manifests, the same way `nico-llm`'s structure test asserts its vendor
//! rule from the sources.
//!
//! What is deliberately **not** asserted: that `nico-metrics` has no HTTP framework *by name*. A
//! transitive arrival would show up as a direct dependency first, and naming every framework would
//! be a list that rots. The direct dependency set is the contract; the boundary commands in
//! `directory-structure.md` cover the closure.

#![expect(
    clippy::expect_used,
    reason = "a structural test that cannot read its manifests has nothing to assert"
)]

/// The manifests this test asserts about, read at compile time.
///
/// `include_str!` rather than a runtime read: the file a test asserts about is known when the test is
/// built, so a missing or renamed manifest is a compile error instead of a test that fails for a
/// reason unrelated to its subject.
const NICO_METRICS: &str = include_str!("../../nico-metrics/Cargo.toml");
const NICO_HTTP: &str = include_str!("../../nico-http/Cargo.toml");
const NICO_CORE: &str = include_str!("../../nico-core/Cargo.toml");

/// The upstream TLS policy layer, read at compile time for the same reason: it is a file whose
/// shape is a promise (`directory-structure.md`, "Upstream TLS: two layers inside `nico-http`").
const TLS_POLICY: &str = include_str!("../../nico-http/src/upstream/tls/policy.rs");

/// Returns the crate names a manifest's `[dependencies]` section actually declares.
///
/// Only dependency **lines** are read, never the section text: the manifests document their edges in
/// comments that name the crates they deliberately avoid ("must not name Pingora or `nico-http`"), so
/// a substring search over the section would fail on the very comments that explain the rule.
///
/// `[dev-dependencies]` is excluded on purpose: a test may use the exporter to assert on rendering,
/// and that edge does not exist in the shipped binary.
fn dependencies(manifest: &str) -> Vec<String> {
    let section = manifest
        .split("[dependencies]")
        .nth(1)
        .expect("the manifest declares a [dependencies] section")
        .split("\n[")
        .next()
        .expect("the section ends at the next table");
    section
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split(['=', '.']).next())
        .map(|name| name.trim().to_owned())
        .collect()
}

/// Asserts `deps` names none of `forbidden`, with a message that states the rule it would break.
///
/// The emptiness check is not defensive noise: "the forbidden name is absent" is trivially true of an
/// empty set, so a parser that silently returned nothing would make every assertion in this file
/// vacuous. Each caller names one dependency it knows the crate must have, which turns a broken parse
/// into a failure instead of a green run.
fn assert_absent(deps: &[String], forbidden: &[&str], required: &str, rule: &str) {
    assert!(
        deps.iter().any(|dep| dep == required),
        "the dependency set was parsed as {deps:?}, which does not even name {required}: the \
         parser is broken, so every \"must not contain\" assertion below would pass vacuously"
    );
    for name in forbidden {
        assert!(
            !deps.iter().any(|dep| dep == name),
            "{rule}: {name} must not be a dependency, but it is. Declared set: {deps:?}"
        );
    }
}

#[test]
fn the_capability_layer_names_no_http_framework_and_no_data_plane() {
    let deps = dependencies(NICO_METRICS);
    assert_absent(
        &deps,
        &["pingora", "nico-http", "hyper", "tokio"],
        "metrics",
        "the capability layer owns no network stack, which is why the crate exists",
    );
}

#[test]
fn the_data_plane_names_neither_the_capability_layer_nor_the_exporter() {
    let deps = dependencies(NICO_HTTP);
    assert_absent(
        &deps,
        &["nico-metrics", "metrics-exporter-prometheus"],
        "metrics",
        "the request path records through the facade; the exporter belongs to the process assembly",
    );
    // The facade itself is required: without it the request path could not record at all.
    assert!(
        deps.iter().any(|dep| dep == "metrics"),
        "nico-http records two request metrics through the facade, so it must name it. Declared \
         set: {deps:?}"
    );
}

#[test]
fn the_kernel_absorbs_no_metrics_dependency() {
    let deps = dependencies(NICO_CORE);
    assert_absent(
        &deps,
        &["metrics", "nico-metrics", "pingora"],
        "thiserror",
        "the kernel carries no observability exporter and no framework",
    );
}

#[test]
fn the_tls_policy_layer_names_no_framework() {
    // The layer is what a future `nico-tls` crate would absorb, and its value is that it can move
    // without the framework coming along. The check is a plain substring, which is stricter than
    // scanning `use` lines: a doc comment that names `pingora` is already the dependency this test
    // exists to prevent, and `adapter.rs` is the file that is allowed to have it.
    assert!(
        !TLS_POLICY.contains("pingora"),
        "`upstream/tls/policy.rs` must stay framework-free; the Pingora mapping belongs in \
         `upstream/tls/adapter.rs`"
    );
    // An empty search over an empty string is vacuous, so the source is proved to be the file the
    // rule is about: a renamed or moved file would otherwise turn the assertion above green.
    assert!(
        TLS_POLICY.contains("fn validate_bundle") && TLS_POLICY.contains("enum RootSource"),
        "the policy source was not read: it must define `RootSource` and `validate_bundle`"
    );
}
