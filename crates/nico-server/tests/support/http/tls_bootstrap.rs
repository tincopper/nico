//! The bootstrap shapes that give an `http.listeners[]` entry a TLS identity.
//!
//! A listener's identity is a block on its entry, not a section-level knob, so the two shapes here
//! differ only in what the `http` section lists: one TLS entry, or one plaintext entry next to one
//! TLS entry. Both name the fixture CA as the **upstream** trust root, so `serve_with`'s startup
//! probe never depends on the host's certificate store — while an entry's `tls` block is the
//! identity this process *presents*, which is the opposite direction and a separate field.
//!
//! `h2c` is an argument of the dual shape rather than a constant: the section-level flag applies to
//! the plaintext entries, and combining it with a TLS entry is exactly what the previous schema
//! refused. Writing the flag here is therefore also how a scenario states that the refusal is gone.

use std::path::Path;

use nico_server::BootstrapConfig;

use super::{fixture_ca, write_and_load};

/// Writes a bootstrap whose only `http.listeners[]` entry terminates TLS.
pub(crate) fn write_bootstrap_tls(
    directory: &Path,
    port: u16,
    cert_file: &Path,
    key_file: &Path,
) -> BootstrapConfig {
    let listeners = format!("  listeners:\n{}", tls_entry(port, cert_file, key_file));
    write_and_load(directory, &render(directory, &listeners))
}

/// Writes a bootstrap with one plaintext listener and one TLS listener, in that order.
///
/// The plaintext entry is first on purpose: the harness talks to the first listener a section names,
/// so `server.address()` stays the plaintext port. The TLS port is never discovered — Pingora does
/// not report the port it bound — it is the port the scenario picked before writing the file.
pub(crate) fn write_bootstrap_dual(
    directory: &Path,
    plain_port: u16,
    tls_port: u16,
    cert_file: &Path,
    key_file: &Path,
    h2c: bool,
) -> BootstrapConfig {
    let listeners = format!(
        "  h2c: {h2c}\n  listeners:\n    - listen: 127.0.0.1:{plain_port}\n{}",
        tls_entry(tls_port, cert_file, key_file)
    );
    write_and_load(directory, &render(directory, &listeners))
}

/// Renders the whole file, with `listeners` as the body of the `http` section.
///
/// One renderer for both shapes: the preamble — a local source, a zero debounce, one worker thread,
/// a short drain and the fixture CA — is a fact of the harness, not something a TLS scenario varies.
fn render(directory: &Path, listeners: &str) -> String {
    format!(
        "\
config_source:
  type: local
  path: {document}
  max_bytes: 1048576
  watch: notify
  debounce_ms: 0
  max_wait_ms: 0
  cache_dir: {cache}
  allow_bootstrap_cache: false
runtime:
  threads: 1
  grace_period_ms: 1
  graceful_shutdown_ms: 1000
proxy:
  tls:
    ca_file: \"{ca}\"
http:
{listeners}",
        document = directory.join("resources.yaml").display(),
        cache = directory.join("cache").display(),
        ca = fixture_ca().display(),
    )
}

/// Renders one `http.listeners[]` entry that presents `cert_file` and `key_file` on `port`.
fn tls_entry(port: u16, cert_file: &Path, key_file: &Path) -> String {
    format!(
        "    - listen: 127.0.0.1:{port}
      tls:
        cert_file: \"{cert}\"
        key_file: \"{key}\"
",
        cert = cert_file.display(),
        key = key_file.display(),
    )
}
