//! Fixtures shared by the configuration validation tests.
//!
//! Every validation test starts from a complete document: the schema rejects a missing section, so
//! a fixture cannot be assembled from fragments. Keeping the envelope and the reusable entries here
//! means a schema change updates one place instead of every string literal, and it keeps each test
//! module focused on the rule it asserts.

/// A section that is present but holds no entries.
pub(super) const NO_ENTRIES: &str = "  []";

/// A complete provider entry.
pub(super) const PROVIDER: &str = concat!(
    "  - name: openai-primary\n",
    "    kind: openai\n",
    "    base_url: https://api.openai.com\n",
    "    credential_ref: env:OPENAI_API_KEY\n",
);

/// A second provider entry whose name differs from [`PROVIDER`].
pub(super) const SECOND_PROVIDER: &str = concat!(
    "  - name: openai-secondary\n",
    "    kind: openai\n",
    "    base_url: https://api.openai.com\n",
    "    credential_ref: env:OPENAI_API_KEY\n",
);

/// A complete upstream entry.
pub(super) const UPSTREAM: &str = concat!(
    "  - name: api-backend\n",
    "    scheme: http\n",
    "    host: 127.0.0.1\n",
    "    port: 8080\n",
);

/// A second upstream entry whose name differs from [`UPSTREAM`].
pub(super) const SECOND_UPSTREAM: &str = concat!(
    "  - name: api-backend-two\n",
    "    scheme: http\n",
    "    host: 127.0.0.1\n",
    "    port: 8081\n",
);

/// Builds a complete document from the bodies of the three collections.
///
/// The bodies are spliced verbatim, so a caller controls indentation and can compose entries
/// without depending on the layout of this file.
pub(super) fn document(providers: &str, upstreams: &str, routes: &str) -> String {
    format!(
        concat!(
            "apiVersion: gateway/v1\n",
            "limits:\n",
            "  max_request_body_bytes: 1048576\n",
            "  request_timeout_ms: 30000\n",
            "providers:\n",
            "{}\n",
            "upstreams:\n",
            "{}\n",
            "routes:\n",
            "{}\n",
        ),
        providers, upstreams, routes
    )
}
