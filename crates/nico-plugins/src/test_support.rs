//! Fixtures shared by this crate's unit tests.
//!
//! A [`PluginSpec`] cannot be built directly — its fields are private and it only exists inside a
//! document that passed validation — so every fixture here starts from a document and reads the
//! entry back. That is deliberate: a test then exercises the same path a published generation does,
//! including the document model's own rules, instead of a shape only a test can produce.
//!
//! The plugin-side double lives in [`context`]: a policy unit test drives the contract, not the data
//! plane's implementation of it.

mod context;

pub(crate) use context::TestContext;
use nico_core::{Document, DocumentError, ParseLimits, PluginSpec};

/// Largest document these fixtures allow themselves.
const DOCUMENT_MAX_BYTES: usize = 64 * 1024;

/// The first route every fixture document declares.
pub(crate) const ROUTE: &str = "api";

/// The second route every fixture document declares.
///
/// Two routes referencing the same plugins is what makes per-route policy state visible: a bucket
/// keyed only by its caller would otherwise be indistinguishable from one keyed by the route too.
pub(crate) const SECOND_ROUTE: &str = "api-two";

/// Builds a document whose `plugins` collection holds `plugins`.
///
/// `plugins` is spliced verbatim, so a caller controls the entries and their indentation without
/// depending on the layout of this file. The single route names `route_plugins`, in the order given,
/// which is what the document model checks against the collection.
pub(crate) fn document(plugins: &str, route_plugins: &[&str]) -> Document {
    Document::parse(
        document_yaml(plugins, route_plugins).as_bytes(),
        ParseLimits::new(DOCUMENT_MAX_BYTES),
    )
    .expect("the fixture document is valid")
}

/// Returns the text [`document`] parses, for a test that must feed the parser itself.
pub(crate) fn document_yaml(plugins: &str, route_plugins: &[&str]) -> String {
    let mut referenced = String::new();
    if !route_plugins.is_empty() {
        referenced.push_str("    plugins:\n");
        for name in route_plugins {
            referenced.push_str("      - ");
            referenced.push_str(name);
            referenced.push('\n');
        }
    }
    format!(
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
            "  - id: api\n",
            "    path_prefix: /api/\n",
            "    upstream: api-backend\n",
            "{}",
            "  - id: api-two\n",
            "    path_prefix: /api/v2/\n",
            "    upstream: api-backend\n",
            "{}",
            "plugins:\n",
            "{}\n",
        ),
        referenced, referenced, plugins
    )
}

/// Parses a document holding `plugins`, expecting the model to refuse it.
///
/// A fixture that turns out to be valid panics instead of being reinterpreted, so a test can only
/// pass because the rule it names was applied.
pub(crate) fn refused(plugins: &str) -> DocumentError {
    match Document::parse(
        document_yaml(plugins, &[]).as_bytes(),
        ParseLimits::new(DOCUMENT_MAX_BYTES),
    ) {
        Ok(_) => panic!("the fixture document was accepted"),
        Err(error) => error,
    }
}

/// Returns the plugin named `name` from `document`.
pub(crate) fn plugin<'a>(document: &'a Document, name: &str) -> &'a PluginSpec {
    document
        .plugins()
        .iter()
        .find(|plugin| plugin.name() == name)
        .expect("the fixture declares this plugin")
}
