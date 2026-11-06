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
/// purpose, not a vendor switch. `protocol/` and `provider/` as directories are exempt because a file
/// named after a vendor lives there; a shared file inside them is listed by path, the same way
/// `provider/route.rs` is.
const VENDOR_FREE: [&str; 10] = [
    "schema.rs",
    "schema",
    "protocol.rs",
    "protocol/capabilities.rs",
    "protocol/facts.rs",
    "provider.rs",
    "provider/route.rs",
    "filter.rs",
    "filter",
    "stream",
];

/// The two module roots the compiler requires to name their concrete modules.
///
/// A registry root must declare `protocol/<vendor>.rs` somewhere, so a vendor name is allowed on a
/// module-wiring line there and nowhere else: the line is a file name, not a decision about a vendor.
const WIRING_ROOTS: [&str; 2] = ["protocol.rs", "provider.rs"];

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

/// Reports whether a line only names a module or imports from one.
///
/// The line must also be a whole declaration — it ends at its `;`, or opens a grouped `use` — so a
/// vendor name cannot ride along in a trailing comment or on a half-written statement.
fn is_module_wiring(line: &str) -> bool {
    let line = line.trim();
    let declares = [
        "mod ",
        "pub mod ",
        "pub(crate) mod ",
        "use ",
        "pub use ",
        "pub(crate) use ",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix));
    declares && (line.ends_with(';') || line.ends_with('{'))
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
        let name = relative.to_string_lossy();
        let root_file = WIRING_ROOTS.contains(&name.as_ref());
        let source = fs::read_to_string(&file).expect("a source file is UTF-8");
        for (index, line) in source.lines().enumerate() {
            let lower = line.to_lowercase();
            for vendor in VENDORS {
                if !lower.contains(vendor) {
                    continue;
                }
                assert!(
                    root_file && is_module_wiring(line),
                    "`{}:{}` names the vendor `{vendor}`: {line}\n\
                     a vendor name belongs only in `protocol/<vendor>.rs` or `provider/<vendor>.rs`, \
                     and only a module-wiring line of a registry root may repeat it",
                    name,
                    index + 1
                );
            }
        }
    }
}

#[test]
fn only_a_whole_module_declaration_may_name_a_vendor() {
    assert!(is_module_wiring("pub mod openai_chat;"));
    assert!(is_module_wiring(" use crate::protocol::openai_chat::{"));
    assert!(
        !is_module_wiring("use std::sync::Arc; // openai"),
        "a trailing comment is not wiring"
    );
    assert!(
        !is_module_wiring("match kind { \"openai\" => 1 }"),
        "a branch is not wiring"
    );
    assert!(!is_module_wiring("let protocol = \"openai.chat\";"));
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
