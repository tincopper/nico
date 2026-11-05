//! Tests for compiling one document into a plan: credentials, kinds and the ordering classes.
//!
//! The subject is publication: what a credential failure says, what an entry naming no implementation
//! is refused with, and how a kind's configuration is validated. The shape of the plan itself — the
//! global chain, chain interning and the request-body mode — is the `plan` submodule's subject.

use nico_core::{Document, ErrorCategory, ParseLimits};
use nico_http::PluginPhase;
use tempfile::TempDir;

use super::{Compiled, PluginRuntime, config_text, phase_label};
use crate::PluginError;

mod plan;

/// Largest document these fixtures allow themselves.
const DOCUMENT_MAX_BYTES: usize = 64 * 1024;

/// The secret every JWT/API-key fixture materializes.
const SECRET: &str = "a-shared-secret-used-by-the-fixture";

/// Parses a document whose `plugins` and `routes` bodies are spliced verbatim.
fn document_with(plugins: &str, routes: &str) -> Document {
    document_with_globals(plugins, "", routes)
}

/// Builds a document that names `global` (a comma-separated list of declared plugin names) at the
/// root, so a test can choose which entries run before routing instead of taking every one.
fn document_with_globals(plugins: &str, global: &str, routes: &str) -> Document {
    let yaml = format!(
        concat!(
            "apiVersion: gateway/v1\n",
            "request:\n",
            "  max_body_bytes: 1024\n",
            "  timeout_ms: 1000\n",
            "providers: []\n",
            "upstreams:\n",
            "  - name: api-backend\n",
            "    scheme: http\n",
            "    host: 127.0.0.1\n",
            "    port: 8080\n",
            "routes:\n",
            "{routes}\n",
            "global_plugins: [{global}]\n",
            "plugins:\n",
            "{plugins}\n",
        ),
        routes = routes,
        global = global,
        plugins = plugins,
    );
    Document::parse(yaml.as_bytes(), ParseLimits::new(DOCUMENT_MAX_BYTES))
        .expect("the fixture document is valid")
}

/// Writes `contents` to a file and returns the `file:` reference naming it.
fn secret_file(directory: &TempDir, contents: &str) -> String {
    let path = directory.path().join("secret");
    std::fs::write(&path, contents).expect("the secret fixture is written");
    format!("file:{}", path.display())
}

/// One `jwt` entry whose key is the credential `secret_ref` names.
fn jwt_entry(secret_ref: &str) -> String {
    format!(
        concat!(
            "  - name: jwt\n",
            "    kind: jwt\n",
            "    config:\n",
            "      keys:\n",
            "        - kid: k1\n",
            "          secret_ref: {}\n",
        ),
        secret_ref
    )
}

/// One `api_key` entry whose only key is the credential `secret_ref` names.
fn api_key_entry(secret_ref: &str) -> String {
    format!(
        concat!(
            "  - name: api-key\n",
            "    kind: api_key\n",
            "    config:\n",
            "      keys:\n",
            "        - name: partner\n",
            "          secret_ref: {}\n",
        ),
        secret_ref
    )
}

/// One `rate_limit` entry named `quota`.
const QUOTA_ENTRY: &str = concat!(
    "  - name: quota\n",
    "    kind: rate_limit\n",
    "    config:\n",
    "      burst: 5\n",
    "      per_second: 2\n",
);

/// One route named `id` that lists `plugins` in the order given.
fn route(id: &str, plugins: &[&str]) -> String {
    let mut yaml = format!("  - id: {id}\n    path_prefix: /{id}/\n    upstream: api-backend\n");
    if plugins.is_empty() {
        return yaml;
    }
    yaml.push_str("    plugins:\n");
    for plugin in plugins {
        yaml.push_str(&format!("      - {plugin}\n"));
    }
    yaml
}

/// Compiles every entry of `document` into the chain elements a route can run.
///
/// The runtime is this crate's own, so the fixture walks the same registry lookup a publication does
/// instead of calling one kind's factory directly.
fn compiled(document: &Document) -> Vec<Compiled> {
    let runtime = PluginRuntime::new();
    document
        .plugins()
        .iter()
        .filter_map(|plugin| {
            runtime
                .compile_plugin(plugin)
                .expect("the fixture compiles")
        })
        .collect()
}

#[test]
fn an_unreadable_credential_names_the_plugin_and_the_reference() {
    let document = document_with(
        &jwt_entry("file:/nonexistent/nico-plugins-fixture/secret"),
        &route("api", &["jwt"]),
    );

    let error = PluginRuntime::new()
        .compile(&document)
        .expect_err("a document whose credential cannot be read is refused");
    let rendered = error.to_string();

    assert!(
        rendered.contains("jwt"),
        "the message must name the plugin: {rendered}"
    );
    assert!(
        rendered.contains("file:/nonexistent/nico-plugins-fixture/secret"),
        "and the reference the operator wrote: {rendered}"
    );
    assert_eq!(
        error.category(),
        ErrorCategory::Upstream,
        "a credential store that cannot be read is a dependency failing"
    );
}

#[test]
fn an_empty_credential_is_refused() {
    // An empty key would make a token signed with nothing verify, so it is a misconfiguration rather
    // than a key, and publication is the last place the difference is visible.
    let directory = TempDir::new().expect("temporary directory");
    let secret = secret_file(&directory, "");
    let document = document_with(&jwt_entry(&secret), &route("api", &["jwt"]));

    let error = PluginRuntime::new()
        .compile(&document)
        .expect_err("an empty credential is refused");

    assert!(
        error.to_string().contains("empty"),
        "the message must say what is wrong, not only that reading failed: {error}"
    );
    assert_eq!(
        error.category(),
        ErrorCategory::Upstream,
        "an unreadable credential is the store failing, not a document the operator mistyped"
    );
}

#[test]
fn an_unknown_configuration_field_is_refused_by_the_kinds_factory() {
    // `config` is opaque to the document model, so the field names are the kind's own and the
    // `deny_unknown_fields` check happens where they are known — still at publication, still keeping
    // the last known good document in service.
    let document = document_with(
        concat!(
            "  - name: quota\n",
            "    kind: rate_limit\n",
            "    config:\n",
            "      burst: 5\n",
            "      per_second: 2\n",
            "      birst: 9\n",
        ),
        &route("api", &["quota"]),
    );

    let error = PluginRuntime::new()
        .compile(&document)
        .expect_err("a field the kind does not have is refused");

    assert!(
        matches!(
            &error,
            PluginError::BuildFailed { plugin, reason, .. }
                if plugin == "quota" && reason.contains("birst")
        ),
        "the failure must name the plugin and the field path, got {error:?}"
    );
    assert_eq!(
        error.category(),
        ErrorCategory::Client,
        "a document the operator wrote wrong is the caller's mistake"
    );
}

#[test]
fn a_build_failure_names_the_field_and_never_repeats_the_value() {
    // A credential is a reference; an entry that writes a value instead is a document fault whose
    // message must not become the second place that value is stored.
    let document = document_with(
        concat!(
            "  - name: api-key\n",
            "    kind: api_key\n",
            "    config:\n",
            "      keys:\n",
            "        - name: partner\n",
            "          secret_ref: s3cret-value\n",
        ),
        &route("api", &["api-key"]),
    );

    let error = PluginRuntime::new()
        .compile(&document)
        .expect_err("a credential that is not a reference is refused");
    let rendered = error.to_string();

    assert!(
        rendered.contains("config.keys[].secret_ref"),
        "the message must name the field: {rendered}"
    );
    assert!(
        !rendered.contains("s3cret-value"),
        "and must not repeat what the operator wrote there: {rendered}"
    );
}

#[test]
fn the_phase_of_a_kind_is_its_ordering_class() {
    // Authentication runs before a limit whatever the document wrote. The class is the kind's
    // declaration, and the label is only how this crate spells it in a compile log.
    assert!(PluginPhase::Authenticate < PluginPhase::Limit);
    assert!(PluginPhase::Limit < PluginPhase::Other);
    assert_eq!(phase_label(PluginPhase::Authenticate), "authenticate");
    assert_eq!(phase_label(PluginPhase::Limit), "limit");
    assert_eq!(
        phase_label(PluginPhase::Other),
        "other",
        "a kind that declares neither class runs after both"
    );
}

#[test]
fn a_plugins_configuration_reaches_it_as_the_json_the_operator_wrote() {
    // `PluginContext::config` promises JSON text, while the document hands the value over as YAML.
    // One conversion at publication, so no stage has to re-serialize a document value.
    let document = document_with(QUOTA_ENTRY, &route("api", &["quota"]));
    let entry = document
        .plugins()
        .first()
        .expect("the fixture declares one plugin");

    assert_eq!(
        config_text(entry)
            .expect("the fixture config is JSON")
            .as_ref(),
        r#"{"burst":5,"per_second":2}"#,
        "the text is what the operator wrote, as JSON"
    );
}

#[test]
fn an_entry_with_no_config_reads_as_the_empty_mapping() {
    // `Value::Null` is how the document stores "wrote nothing"; a plugin reads `{}` so a kind with
    // every field defaulted can parse its own configuration text.
    let document = document_with(
        "  - name: quota\n    kind: rate_limit\n",
        &route("api", &["quota"]),
    );

    assert_eq!(
        config_text(document.plugins().first().expect("one plugin"))
            .expect("an absent config is representable")
            .as_ref(),
        "{}"
    );
}

#[test]
fn an_unknown_kind_is_refused_and_the_message_names_it() {
    // The document model cannot answer this: `kind` is a string there. The registry is this crate, so
    // "no implementation answers to it" has to refuse the document — skipping the entry would leave a
    // route running fewer policies than it names.
    let document = document_with(
        concat!(
            "  - name: wasm-policy\n",
            "    kind: wasm\n",
            "    config:\n",
            "      module_ref: file:/policies/policy.wasm\n",
        ),
        &route("api", &["wasm-policy"]),
    );

    let error = PluginRuntime::new()
        .compile(&document)
        .expect_err("this build implements no `wasm` kind");

    assert!(
        matches!(&error, PluginError::UnknownKind { plugin, kind } if plugin == "wasm-policy" && kind == "wasm"),
        "expected UnknownKind naming the entry and the kind, got {error:?}"
    );
    assert!(
        error.to_string().contains("wasm"),
        "the operator must see the spelling that failed: {error}"
    );
    assert_eq!(
        error.category(),
        ErrorCategory::Client,
        "a kind nothing answers to is a document fault"
    );
}
