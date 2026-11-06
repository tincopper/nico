//! The module tree's structural promises, checked against the sources themselves.
//!
//! Two rules are cheap to state and easy to break by accident: the canonical model and the exchange
//! never learn a vendor's name, and the HTTP data plane never depends on this crate. Both are
//! asserted here so a violating change fails a test rather than a review.

#![expect(
    clippy::expect_used,
    reason = "a structural test that cannot read its sources has nothing to assert"
)]

use std::{
    fs,
    path::{Path, PathBuf},
};

/// Names that must not appear outside a `protocol/<vendor>.rs` or `provider/<vendor>.rs` module.
const VENDORS: [&str; 5] = ["openai", "anthropic", "gemini", "claude", "gpt"];

/// Files, and directories walked recursively, that must stay vendor-free.
///
/// `lib.rs` is deliberately absent: the crate root names the protocols the crate bridges, which is its
/// purpose, not a vendor switch. `stream/` is listed before its first file exists so the rule starts
/// applying the moment `stream/sse.rs` lands. `protocol/` as a directory is exempt because a file
/// named after a vendor lives there; a shared file inside it is listed by path, the same way
/// `provider/route.rs` is.
const VENDOR_FREE: [&str; 9] = [
    "schema.rs",
    "schema",
    "protocol.rs",
    "protocol/facts.rs",
    "provider.rs",
    "provider/route.rs",
    "filter.rs",
    "filter",
    "stream",
];

fn source_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn collect(path: &Path, files: &mut Vec<PathBuf>) {
    // A module that has not landed yet is skipped instead of failing the scan; the entry starts
    // enforcing the rule as soon as the file exists.
    if !path.exists() {
        return;
    }
    if path.is_dir() {
        let entries = fs::read_dir(path).expect("the source directory is readable");
        for entry in entries {
            collect(&entry.expect("a directory entry is readable").path(), files);
        }
        return;
    }
    files.push(path.to_path_buf());
}

#[test]
fn the_canonical_model_and_the_exchange_name_no_vendor() {
    let root = source_root();
    let mut files = Vec::new();
    for entry in VENDOR_FREE {
        collect(&root.join(entry), &mut files);
    }
    assert!(!files.is_empty(), "the scan must look at real files");

    for file in files {
        let relative = file.strip_prefix(&root).expect("the file lives under src/");
        let source = fs::read_to_string(&file).expect("a source file is UTF-8");
        let lower = source.to_lowercase();
        for vendor in VENDORS {
            assert!(
                !lower.contains(vendor),
                "`{}` names the vendor `{vendor}`; a vendor name belongs only in `protocol/<vendor>.rs` or `provider/<vendor>.rs`",
                relative.display()
            );
        }
    }
}

#[test]
fn the_http_data_plane_does_not_depend_on_this_crate() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../nico-http/Cargo.toml");
    let text = fs::read_to_string(&manifest).expect("the HTTP crate's manifest is readable");
    assert!(
        !text.contains("nico-llm"),
        "`nico-http` must reach LLM support through its own byte-level traits, never through this crate"
    );
}
