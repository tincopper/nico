//! The README's `quota_store` snippets parse with the real schema.
//!
//! Documentation drifts; the schema does not. These two probes load the exact YAML the README
//! documents through [`BootstrapConfig::load`], so a schema change that would break every
//! deployment copying the README fails here first.

// The panic-free policy protects request paths. A test helper that cannot build its fixture has
// nothing useful to return, and clippy only recognises the lint exemption inside `#[test]`
// functions, not in the helpers they call.
#![expect(
    clippy::expect_used,
    reason = "test helpers fail loudly when their fixture cannot be built"
)]

use nico_server::BootstrapConfig;

fn load(document: &str) -> Result<BootstrapConfig, nico_server::BootstrapError> {
    let directory = tempfile::TempDir::new().expect("temp dir");
    let path = directory.path().join("bootstrap.yaml");
    std::fs::write(&path, document).expect("write");
    BootstrapConfig::load(&path)
}

fn base(section: &str) -> String {
    format!(
        "config_source:\n  type: local\n  path: ./config/resources.yaml\n  max_bytes: 4096\n  watch: notify\n  debounce_ms: 100\n  max_wait_ms: 500\n  cache_dir: ./.tmp/cache\n  allow_bootstrap_cache: false\nhttp:\n  listeners:\n    - listen: 127.0.0.1:8080\n{section}"
    )
}

#[test]
fn the_readme_redis_snippet_parses() {
    let document = base(
        "quota_store:\n  type: redis\n  key_prefix: nico/quota\n  on_unavailable: fail-closed\n  endpoints:\n    - \"rediss://quota.internal:6379\"\n",
    );
    let bootstrap = load(&document).expect("the README's redis snippet parses");
    assert!(matches!(
        bootstrap.quota_store(),
        nico_server::bootstrap::QuotaStoreSelection::Redis { .. }
    ));
}

#[cfg(feature = "etcd")]
#[test]
fn the_readme_etcd_snippet_parses() {
    let document = base(
        "quota_store:\n  type: etcd\n  key_prefix: nico/quota\n  on_unavailable: fail-closed\n  endpoints:\n    - \"https://etcd-0.internal:2379\"\n  tls:\n    ca_file: /etc/nico/etcd-ca.pem\n  auth:\n    user: nico\n    password_ref: env:ETCD_PASSWORD\n",
    );
    // The snippet's ca_file does not exist in the test environment, so the parse refuses on the
    // TLS file — which still proves the *schema* accepted every field. Match on the field path.
    let error = load(&document).expect_err("the fixture's ca_file does not exist here");
    assert!(
        format!("{error:?}").contains("quota_store.etcd.tls.ca_file"),
        "the schema accepted the snippet and the refusal names the unreadable file: {error:?}"
    );
}
