//! The shipped deployment files are parsed by the same code the running process uses.
//!
//! A Dockerfile can build a perfectly good image around a bootstrap file that fails at startup.
//! These tests are the static half of that check: the image needs a daemon to prove it starts, but
//! the configuration shape can be proven here without one.

use std::path::Path;

use nico_core::{Document, ParseLimits};
use nico_server::BootstrapConfig;

/// Returns the path to a file at the workspace root, independent of the test's cwd.
fn root(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(name)
}

#[test]
fn the_shipped_bootstrap_loads() {
    let bootstrap = BootstrapConfig::load(root("deploy/bootstrap.yaml"))
        .expect("the shipped bootstrap is a valid bootstrap document");

    assert_eq!(bootstrap.cache_dir(), Path::new("/var/lib/nico/cache"));
    assert_eq!(
        bootstrap.metrics().map(|metrics| metrics.listen()),
        Some("0.0.0.0:9090"),
        "the health probe has a listener to reach"
    );
}

#[test]
fn the_shipped_resources_document_parses() {
    let bytes = std::fs::read(root("deploy/resources.yaml"))
        .expect("the shipped resources document exists");
    let document = Document::parse(&bytes, ParseLimits::new(1 << 20))
        .expect("the shipped resources document parses");

    assert_eq!(document.routes().len(), 1);
    assert!(
        document
            .routes()
            .iter()
            .all(|route| matches!(route.target(), nico_core::RouteTarget::Upstream(_))),
        "no provider route: the traffic image can publish this document too"
    );
}

#[test]
fn the_bootstrap_points_at_the_shipped_document() {
    // The bootstrap names an absolute path in the image; the check here is that the basename is the
    // file we actually ship, so a rename cannot silently leave the image pointing at nothing.
    let bootstrap =
        BootstrapConfig::load(root("deploy/bootstrap.yaml")).expect("the shipped bootstrap loads");
    let nico_server::bootstrap::SourceSelection::Local { path, .. } = bootstrap.source() else {
        panic!("the shipped bootstrap selects the local source");
    };
    assert!(
        path.file_name()
            .is_some_and(|name| name == "resources.yaml"),
        "the bootstrap names the shipped document: {path:?}"
    );
}
