//! Rule tests for [`super::validate`].
//!
//! Each fixture differs from a valid document in exactly one field, so a failure names the rule that
//! broke. Syntax that belongs to another stage (unknown scheme, unknown field) is covered by the
//! parse-level tests beside [`Document`].

use super::*;
use crate::{
    config::{
        bounds::ParseLimits,
        document::Document,
        test_support::{NO_ENTRIES, PROVIDER, UPSTREAM, document, document_with_plugins},
    },
    error::ErrorCategory,
};

fn parse(document: &str) -> Result<Document, DocumentError> {
    Document::parse(document.as_bytes(), ParseLimits::new(16 * 1024))
}

/// Parses a document whose routes hold `routes`, with the fixture provider and upstream defined.
fn with_routes(routes: &str) -> Result<Document, DocumentError> {
    parse(&document(PROVIDER, UPSTREAM, routes))
}

/// Parses a document whose `plugins` collection holds `plugins`, and nothing else.
fn with_plugins(plugins: &str) -> Result<Document, DocumentError> {
    parse(&document_with_plugins(
        NO_ENTRIES, NO_ENTRIES, NO_ENTRIES, plugins,
    ))
}

/// Builds a single provider entry whose base URL and credential reference are variable.
fn provider_entry(base_url: &str, credential_ref: &str) -> String {
    format!(
        concat!(
            "  - name: openai-primary\n",
            "    kind: openai\n",
            "    base_url: {}\n",
            "    credential_ref: {}\n",
        ),
        base_url, credential_ref
    )
}

/// A route that forwards to the fixture upstream.
fn route(id: &str, path_prefix: &str) -> String {
    format!(
        concat!(
            "  - id: {}\n",
            "    path_prefix: {}\n",
            "    upstream: api-backend\n",
        ),
        id, path_prefix
    )
}

mod hosts;
mod matches;
mod names;
mod numeric;
mod paths;
mod plugins;
mod providers;
mod upstreams;
