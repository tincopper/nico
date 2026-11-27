//! The `quota_store` section: omitted is local, written is closed, and the failure policy is
//! required for every shared backend.

use super::*;

/// A bootstrap document with a `quota_store` section appended.
fn with_quota_store(section: &str) -> String {
    format!(
        "{}{}http:\n  listeners:\n    - listen: 127.0.0.1:8080\n",
        config_source("./resources.yaml", "notify", 100, 500),
        section
    )
}

/// Loads a fixture and asserts the given field is refused, returning the error's wording.
fn refused(document: &str, field: &str) -> String {
    match load_document(document).expect_err("the fixture is refused") {
        BootstrapError::InvalidField {
            field: named,
            reason,
        } => {
            assert_eq!(named, field, "the refusal names the written field");
            reason.to_string()
        }
        error => panic!("expected InvalidField for {field}, got {error:?}"),
    }
}

#[test]
fn an_omitted_section_is_the_local_store() {
    let bootstrap = load_document(&document("./resources.yaml", "notify", 100, 500))
        .expect("the plain document parses");
    assert_eq!(bootstrap.quota_store(), &QuotaStoreSelection::Local);
    assert_eq!(bootstrap.quota_policy(), UnavailablePolicy::FailClosed);
}

#[cfg(feature = "etcd")]
#[test]
fn an_etcd_store_is_accepted_with_its_policy_and_prefix() {
    let text = with_quota_store(
        "quota_store:\n  type: etcd\n  key_prefix: nico/quota\n  on_unavailable: fail-closed\n  endpoints:\n    - 127.0.0.1:2379\n",
    );
    let bootstrap = load_document(&text).expect("the etcd section parses");
    let QuotaStoreSelection::Etcd {
        endpoints,
        key_prefix,
        on_unavailable,
        ..
    } = bootstrap.quota_store()
    else {
        panic!("the document selects the etcd quota store");
    };
    assert_eq!(endpoints, &["127.0.0.1:2379".to_owned()]);
    assert_eq!(key_prefix, "nico/quota");
    assert_eq!(*on_unavailable, UnavailablePolicy::FailClosed);
    assert_eq!(bootstrap.quota_policy(), UnavailablePolicy::FailClosed);
}

#[test]
fn a_redis_store_is_accepted_with_fail_open() {
    let text = with_quota_store(
        "quota_store:\n  type: redis\n  key_prefix: \"nico:quota:\"\n  on_unavailable: fail-open\n  endpoints:\n    - \"redis://127.0.0.1:6379\"\n",
    );
    let bootstrap = load_document(&text).expect("the redis section parses");
    let QuotaStoreSelection::Redis {
        endpoints,
        key_prefix,
        on_unavailable,
    } = bootstrap.quota_store()
    else {
        panic!("the document selects the redis quota store");
    };
    assert_eq!(endpoints, &["redis://127.0.0.1:6379".to_owned()]);
    assert_eq!(key_prefix, "nico:quota:");
    assert_eq!(*on_unavailable, UnavailablePolicy::FailOpen);
}

#[test]
fn an_unknown_backend_type_is_refused() {
    let error = refused(
        &with_quota_store(
            "quota_store:\n  type: nacos\n  key_prefix: p\n  on_unavailable: fail-closed\n  endpoints:\n    - x:1\n",
        ),
        "quota_store.type",
    );
    assert!(error.contains("etcd"), "the refusal names the served types");
}

#[test]
fn a_missing_failure_policy_is_a_parse_error_not_a_default() {
    // `on_unavailable` is required: a shared store without a stated policy is a config the
    // schema refuses, never a silent fail-open.
    let error = load_document(&with_quota_store(
        "quota_store:\n  type: redis\n  key_prefix: p\n  endpoints:\n    - redis://127.0.0.1:6379\n",
    ))
    .expect_err("the policy is required");
    assert!(matches!(error, BootstrapError::Parse { .. }));
}

#[test]
fn an_unknown_policy_value_is_refused_naming_the_field() {
    let error = refused(
        &with_quota_store(
            "quota_store:\n  type: redis\n  key_prefix: p\n  on_unavailable: maybe\n  endpoints:\n    - redis://127.0.0.1:6379\n",
        ),
        "quota_store.on_unavailable",
    );
    assert!(error.contains("fail-closed") && error.contains("fail-open"));
}

#[test]
fn an_empty_key_prefix_is_refused() {
    refused(
        &with_quota_store(
            "quota_store:\n  type: redis\n  key_prefix: \"\"\n  on_unavailable: fail-closed\n  endpoints:\n    - redis://127.0.0.1:6379\n",
        ),
        "quota_store.key_prefix",
    );
}

#[test]
fn a_control_character_in_the_key_prefix_is_refused() {
    refused(
        &with_quota_store(
            "quota_store:\n  type: redis\n  key_prefix: \"nico\\tquota\"\n  on_unavailable: fail-closed\n  endpoints:\n    - redis://127.0.0.1:6379\n",
        ),
        "quota_store.key_prefix",
    );
}

#[test]
fn an_empty_endpoint_list_is_refused() {
    refused(
        &with_quota_store(
            "quota_store:\n  type: redis\n  key_prefix: p\n  on_unavailable: fail-closed\n  endpoints: []\n",
        ),
        "quota_store.redis.endpoints",
    );
}

#[test]
fn a_field_for_the_wrong_backend_is_an_unknown_field_error() {
    // `path` belongs to the local config source, not to a redis quota store: the closed shape
    // refuses it instead of ignoring it.
    let error = load_document(&with_quota_store(
        "quota_store:\n  type: redis\n  path: ./x\n  key_prefix: p\n  on_unavailable: fail-closed\n  endpoints:\n    - redis://127.0.0.1:6379\n",
    ))
    .expect_err("the wrong field is refused");
    assert!(matches!(error, BootstrapError::Parse { .. }));
}

#[cfg(feature = "etcd")]
#[test]
fn an_etcd_tls_block_missing_its_ca_file_is_refused_by_quota_field() {
    refused(
        &with_quota_store(
            "quota_store:\n  type: etcd\n  key_prefix: p\n  on_unavailable: fail-closed\n  endpoints:\n    - 127.0.0.1:2379\n  tls:\n    cert_file: /tmp/nico-no-such-cert.pem\n    key_file: /tmp/nico-no-such-key.pem\n",
        ),
        "quota_store.etcd.tls.ca_file",
    );
}

#[cfg(feature = "etcd")]
#[test]
fn an_etcd_auth_block_with_a_value_password_is_refused() {
    refused(
        &with_quota_store(
            "quota_store:\n  type: etcd\n  key_prefix: p\n  on_unavailable: fail-closed\n  endpoints:\n    - 127.0.0.1:2379\n  auth:\n    user: root\n    password_ref: plain-secret\n",
        ),
        "quota_store.etcd.auth.password_ref",
    );
}
