//! Parse-level tests for the `plugins` collection and the routes that reference it.
//!
//! These sit beside the other parse tests because the subject is the document schema: the envelope's
//! fields, the fact that `kind` is an opaque string and `config` an opaque value, and the accessors
//! that expose a valid entry. Range, name and reference rules are covered by the `semantic` and
//! `reference` suites; what a kind accepts under `config` is covered where that kind is implemented.

use super::*;
use crate::config::ErrorResponse;

#[test]
fn parse_exposes_plugins() {
    let document = "\
apiVersion: gateway/v1
request:
  max_body_bytes: 1048576
  timeout_ms: 30000
plugins:
  - name: public-jwt
    kind: jwt
    config:
      keys:
        - kid: k1
          secret_ref: env:JWT_SECRET
      issuer: https://id.example
      audience: api
      leeway_seconds: 5
    error_response:
      status: 403
      body: { error: denied }
  - name: api-quota
    kind: rate_limit
    enabled: false
    buffers_body: true
    config:
      burst: 100
      per_second: 20
      identity: ip
providers: []
upstreams:
  - name: api-backend
    scheme: http
    host: 127.0.0.1
    port: 8080
routes:
  - id: api-public-v1
    path_prefix: /api/
    upstream: api-backend
    plugins: [public-jwt, api-quota]
";
    let document =
        Document::parse(document.as_bytes(), ParseLimits::new(4096)).expect("fixture is valid");

    let jwt = &document.plugins()[0];
    assert_eq!(jwt.name(), "public-jwt");
    assert_eq!(jwt.kind(), "jwt");
    assert!(jwt.is_enabled());
    assert!(!jwt.buffers_body());
    assert_eq!(
        jwt.error_response().and_then(ErrorResponse::status),
        Some(403)
    );
    assert!(
        jwt.config().get("keys").is_some(),
        "the kind's own fields arrive as written: {:?}",
        jwt.config()
    );

    let quota = &document.plugins()[1];
    assert_eq!(quota.kind(), "rate_limit");
    assert!(
        !quota.is_enabled(),
        "a disabled entry is still a declared plugin"
    );
    assert!(quota.buffers_body());
    assert_eq!(quota.error_response(), None);
    assert_eq!(
        document.routes()[0].plugins(),
        ["public-jwt".to_owned(), "api-quota".to_owned()],
        "a disabled plugin can still be referenced"
    );
}

#[test]
fn parse_accepts_an_unknown_plugin_kind() {
    // The kinds this build implements are a registry, not a document-model enum: an entry naming a
    // kind nothing answers to is refused when the document is *compiled*, and the message names the
    // kind. Teaching the schema about it would make every new plugin a kernel change.
    let document = "\
apiVersion: gateway/v1
request:
  max_body_bytes: 1024
  timeout_ms: 1000
plugins:
  - name: wasm-policy
    kind: wasm
    config:
      module_ref: file:/policies/policy.wasm
providers: []
upstreams: []
routes: []
";
    let document = Document::parse(document.as_bytes(), ParseLimits::new(4096))
        .expect("the document model does not know kinds");
    assert_eq!(document.plugins()[0].kind(), "wasm");
    assert_eq!(
        document.plugins()[0]
            .config()
            .get("module_ref")
            .and_then(serde_norway::Value::as_str),
        Some("file:/policies/policy.wasm")
    );
}

#[test]
fn parse_rejects_a_field_the_plugin_envelope_does_not_have() {
    let document = "\
apiVersion: gateway/v1
request:
  max_body_bytes: 1024
  timeout_ms: 1000
plugins:
  - name: public-jwt
    kind: jwt
    keys: [{ secret_ref: env:JWT_SECRET }]
providers: []
upstreams: []
routes: []
";
    let error = Document::parse(document.as_bytes(), ParseLimits::new(4096))
        .expect_err("`keys` belongs under `config`");
    assert!(
        matches!(error, DocumentError::Yaml { .. }),
        "expected a schema error, got {error:?}"
    );
    assert_eq!(error.category(), ErrorCategory::Client);
}

#[test]
fn parse_rejects_a_kind_that_names_nothing() {
    let document = "\
apiVersion: gateway/v1
request:
  max_body_bytes: 1024
  timeout_ms: 1000
plugins:
  - name: public-jwt
    kind: \"  \"
providers: []
upstreams: []
routes: []
";
    let error = Document::parse(document.as_bytes(), ParseLimits::new(4096))
        .expect_err("an entry that names no implementation is unusable");
    assert!(
        matches!(
            error,
            DocumentError::InvalidField {
                field: "plugins[].kind",
                ..
            }
        ),
        "expected InvalidField for the kind, got {error:?}"
    );
}

#[test]
fn parse_accepts_an_arbitrary_configuration_value() {
    // A kind's configuration is opaque here, so a shape no built-in kind accepts still parses: the
    // refusal belongs to the implementation that reads it.
    let document = "\
apiVersion: gateway/v1
request:
  max_body_bytes: 1024
  timeout_ms: 1000
plugins:
  - name: partner-keys
    kind: api_key
    config: any-scalar-a-kind-may-reject
providers: []
upstreams: []
routes: []
";
    let document =
        Document::parse(document.as_bytes(), ParseLimits::new(4096)).expect("fixture is valid");

    assert_eq!(
        document.plugins()[0].config(),
        &serde_norway::Value::String("any-scalar-a-kind-may-reject".to_owned())
    );
}

#[test]
fn parse_exposes_the_global_plugins_in_document_order() {
    let document = "\
apiVersion: gateway/v1
request:
  max_body_bytes: 1024
  timeout_ms: 1000
plugins:
  - name: public-jwt
    kind: jwt
    config: { keys: [{ kid: k1, secret_ref: env:JWT_SECRET }] }
  - name: api-quota
    kind: rate_limit
    config: { burst: 10, per_second: 5 }
global_plugins: [api-quota, public-jwt]
providers: []
upstreams: []
routes: []
";
    let document =
        Document::parse(document.as_bytes(), ParseLimits::new(4096)).expect("fixture is valid");

    assert_eq!(document.global_plugins(), ["api-quota", "public-jwt"]);
}

#[test]
fn a_document_without_global_plugins_runs_none_before_routing() {
    let document = "\
apiVersion: gateway/v1
request:
  max_body_bytes: 1024
  timeout_ms: 1000
plugins:
  - name: public-jwt
    kind: jwt
    config: { keys: [{ kid: k1, secret_ref: env:JWT_SECRET }] }
providers: []
upstreams: []
routes: []
";
    let document =
        Document::parse(document.as_bytes(), ParseLimits::new(4096)).expect("fixture is valid");

    assert!(
        document.global_plugins().is_empty(),
        "declaring a plugin is not a decision to run it globally"
    );
}

#[test]
fn parse_rejects_a_global_plugin_that_is_not_declared() {
    let document = "\
apiVersion: gateway/v1
request:
  max_body_bytes: 1024
  timeout_ms: 1000
plugins: []
global_plugins: [edge-firewall]
providers: []
upstreams: []
routes: []
";
    let error = Document::parse(document.as_bytes(), ParseLimits::new(4096))
        .expect_err("the name is not declared");
    assert!(
        matches!(
            error,
            DocumentError::ReferenceUnknown {
                kind: "plugin",
                ref name,
            } if name == "edge-firewall"
        ),
        "expected ReferenceUnknown for the plugin, got {error:?}"
    );
}

#[test]
fn parse_rejects_a_global_plugin_listed_twice() {
    let document = "\
apiVersion: gateway/v1
request:
  max_body_bytes: 1024
  timeout_ms: 1000
plugins:
  - name: edge-firewall
    kind: api_key
    config: { keys: [{ name: edge, secret_ref: env:EDGE_KEY }] }
global_plugins: [edge-firewall, edge-firewall]
providers: []
upstreams: []
routes: []
";
    let error = Document::parse(document.as_bytes(), ParseLimits::new(4096))
        .expect_err("the same plugin cannot run twice in one scope");
    assert!(
        matches!(
            error,
            DocumentError::DuplicateId {
                kind: "global-plugin",
                ..
            }
        ),
        "expected DuplicateId for the global list, got {error:?}"
    );
}
