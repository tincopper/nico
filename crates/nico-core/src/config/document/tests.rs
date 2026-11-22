//! Parse-level tests for [`Document`](super::Document).
//!
//! Kept beside the module rather than inline because the fixtures alone are longer than the model:
//! every case here starts from a complete document and asserts the error variant, which is the
//! behavior a caller depends on.
//!
//! The shipped example document is checked by [`llm`], next to the other assertions about the LLM
//! route it contains.

use super::*;
use crate::{
    config::{
        bounds::{Bound, SchemaBounds},
        request::DEFAULT_MAX_RESPONSE_BYTES,
        route::{RouteMode, RouteTarget},
        test_support::{NO_ENTRIES, UPSTREAM},
        upstream::Scheme,
    },
    error::ErrorCategory,
};

const VALID: &str = "\
apiVersion: gateway/v1
request:
  max_body_bytes: 1048576
  timeout_ms: 30000
plugins: []
providers: []
upstreams: []
routes: []
";

fn parse_valid() -> Document {
    Document::parse(VALID.as_bytes(), ParseLimits::new(1024 * 1024)).expect("fixture is valid")
}

/// Builds a schema-valid document with the requested values.
///
/// The literal starts each line in column 0 on purpose: a `\`-continued literal would strip
/// the indentation that makes `max_body_bytes` a child of `request`.
fn yaml_document(api_version: &str, max_body_bytes: u32, timeout_ms: u32) -> String {
    format!(
        "\
apiVersion: {api_version}
request:
  max_body_bytes: {max_body_bytes}
  timeout_ms: {timeout_ms}
plugins: []
providers: []
upstreams: []
routes: []
"
    )
}

mod parse {
    use super::*;
    #[test]
    fn parse_accepts_a_valid_document() {
        let document = parse_valid();
        assert_eq!(document.api_version(), API_VERSION);
        assert_eq!(document.request().max_body_bytes(), 1_048_576);
        assert_eq!(document.request().timeout_ms(), 30_000);
        assert!(document.providers().is_empty());
        assert!(document.upstreams().is_empty());
        assert!(document.routes().is_empty());
    }

    #[test]
    fn parse_exposes_routes_upstreams_and_providers() {
        let document = "\
apiVersion: gateway/v1
request:
  max_body_bytes: 1048576
  timeout_ms: 30000
plugins: []
providers:
  - name: openai-primary
    kind: openai
    base_url: https://api.openai.com
    credential_ref: env:OPENAI_API_KEY
upstreams:
  - name: api-backend
    scheme: http
    host: 127.0.0.1
    port: 8080
routes:
  - id: api-public-v1
    path_prefix: /api/
    upstream: api-backend
  - id: llm-chat
    path_prefix: /v1/chat/
    provider: openai-primary
    protocol: openai.chat
    timeout_ms: 120000
";
        let document =
            Document::parse(document.as_bytes(), ParseLimits::new(4096)).expect("fixture is valid");

        let provider = &document.providers()[0];
        assert_eq!(provider.name(), "openai-primary");
        assert_eq!(provider.kind(), "openai");
        assert_eq!(provider.base_url(), "https://api.openai.com");
        assert_eq!(provider.credential_ref(), "env:OPENAI_API_KEY");
        assert!(provider.model_allowlist().is_empty());
        assert!(
            provider.model_map().is_empty(),
            "without a map the caller's model name is sent unchanged"
        );
        assert_eq!(
            provider.max_output_tokens(),
            None,
            "without the field the endpoint imposes no output ceiling"
        );

        let upstream = &document.upstreams()[0];
        assert_eq!(upstream.name(), "api-backend");
        assert_eq!(upstream.scheme(), Scheme::Http);
        assert_eq!(upstream.host(), "127.0.0.1");
        assert_eq!(upstream.port(), 8080);

        let routes = document.routes();
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].id(), "api-public-v1");
        assert_eq!(routes[0].host(), None);
        assert_eq!(routes[0].path_prefix(), "/api/");
        assert_eq!(
            routes[0].target(),
            &RouteTarget::Upstream("api-backend".to_owned())
        );
        assert_eq!(routes[1].target().name(), "openai-primary");
        assert_eq!(routes[1].timeout_ms(), Some(120_000));
        assert_eq!(routes[1].max_body_bytes(), None);
        assert_eq!(routes[1].protocol(), Some("openai.chat"));
        assert_eq!(routes[1].mode(), RouteMode::Convert);
        assert_eq!(routes[1].max_response_bytes(), None);
        // A route that writes no response bound inherits the document's, not an unlimited one.
        assert_eq!(
            document.request().max_response_bytes(),
            DEFAULT_MAX_RESPONSE_BYTES
        );
    }

    #[test]
    fn parse_rejects_an_unknown_upstream_scheme() {
        let document = "\
apiVersion: gateway/v1
request:
  max_body_bytes: 1024
  timeout_ms: 1000
plugins: []
providers: []
upstreams:
  - name: api-backend
    scheme: grpc
    host: 127.0.0.1
    port: 8080
routes: []
";
        let error = Document::parse(document.as_bytes(), ParseLimits::new(4096))
            .expect_err("the scheme is a closed set");
        assert!(
            matches!(error, DocumentError::Yaml { .. }),
            "expected Yaml error, got {error:?}"
        );
        assert_eq!(error.category(), ErrorCategory::Client);
    }

    #[test]
    fn parse_does_not_decide_which_provider_kinds_exist() {
        // A kind is an opaque registry key: the document carries the word, and the crate that
        // assembles the adapters answers whether it names one. Deciding here would make adding a
        // provider a change to this crate, which is exactly what the dependency rule forbids.
        let document = "\
apiVersion: gateway/v1
request:
  max_body_bytes: 1024
  timeout_ms: 1000
plugins: []
providers:
  - name: openai-primary
    kind: anthropic
    base_url: https://api.anthropic.com
    credential_ref: env:ANTHROPIC_API_KEY
upstreams: []
routes: []
";
        let parsed = Document::parse(document.as_bytes(), ParseLimits::new(4096))
            .expect("an unknown kind is a registry question, not a schema error");
        assert_eq!(
            parsed.providers()[0].kind(),
            "anthropic",
            "the word the operator wrote is the key the registry is asked for"
        );
    }

    #[test]
    fn parse_rejects_a_provider_kind_that_is_not_a_token() {
        // Shape is this crate's business: the word travels in a log field and is used as a key, so it
        // is one line of printable ASCII with no spaces, whatever it names.
        let document = "\
apiVersion: gateway/v1
request:
  max_body_bytes: 1024
  timeout_ms: 1000
plugins: []
providers:
  - name: openai-primary
    kind: \"two words\"
    base_url: https://api.openai.com
    credential_ref: env:OPENAI_API_KEY
upstreams: []
routes: []
";
        let error = Document::parse(document.as_bytes(), ParseLimits::new(4096))
            .expect_err("a kind with a space is not a token");
        assert!(
            matches!(error, DocumentError::InvalidField { .. }),
            "expected InvalidField, got {error:?}"
        );
        assert_eq!(error.category(), ErrorCategory::Client);
    }

    #[test]
    fn parse_defaults_an_omitted_model_allowlist_to_empty() {
        let document = "\
apiVersion: gateway/v1
request:
  max_body_bytes: 1048576
  timeout_ms: 30000
plugins: []
providers:
  - name: openai-primary
    kind: openai
    base_url: https://api.openai.com
    credential_ref: env:OPENAI_API_KEY
upstreams: []
routes: []
";
        let document =
            Document::parse(document.as_bytes(), ParseLimits::new(4096)).expect("fixture is valid");
        assert!(document.providers()[0].model_allowlist().is_empty());
    }

    #[test]
    fn parse_preserves_an_explicit_model_allowlist() {
        let document = "\
apiVersion: gateway/v1
request:
  max_body_bytes: 1048576
  timeout_ms: 30000
plugins: []
providers:
  - name: openai-primary
    kind: openai
    base_url: https://api.openai.com
    credential_ref: env:OPENAI_API_KEY
    model_allowlist: [gpt-4o, gpt-4o-mini]
upstreams: []
routes: []
";
        let document =
            Document::parse(document.as_bytes(), ParseLimits::new(4096)).expect("fixture is valid");
        assert_eq!(
            document.providers()[0].model_allowlist(),
            ["gpt-4o".to_owned(), "gpt-4o-mini".to_owned()]
        );
    }

    #[test]
    fn parse_rejects_unknown_fields() {
        let mut document = yaml_document(API_VERSION, 1024, 1000);
        document.push_str("listeners: []\n");
        let error = Document::parse(document.as_bytes(), ParseLimits::new(4096))
            .expect_err("unknown field");
        assert!(
            matches!(error, DocumentError::Yaml { .. }),
            "expected Yaml error, got {error:?}"
        );
        assert_eq!(error.category(), ErrorCategory::Client);
    }

    #[test]
    fn parse_rejects_unknown_route_fields() {
        let document = "\
apiVersion: gateway/v1
request:
  max_body_bytes: 1024
  timeout_ms: 1000
plugins: []
providers: []
upstreams: []
routes:
  - id: api-public-v1
    path_prefix: /api/
    upstream: api-backend
    retries: 3
";
        let error = Document::parse(document.as_bytes(), ParseLimits::new(4096))
            .expect_err("unknown route field");
        assert!(
            matches!(error, DocumentError::Yaml { .. }),
            "expected Yaml error, got {error:?}"
        );
    }

    #[test]
    fn parse_rejects_unknown_limit_fields() {
        let document = "\
apiVersion: gateway/v1
request:
  max_body_bytes: 1024
  timeout_ms: 1000
  max_connections: 10
plugins: []
providers: []
upstreams: []
routes: []
";
        let error = Document::parse(document.as_bytes(), ParseLimits::new(4096))
            .expect_err("unknown field");
        assert!(
            matches!(error, DocumentError::Yaml { .. }),
            "expected Yaml error, got {error:?}"
        );
    }

    #[test]
    fn parse_rejects_a_missing_section() {
        for section in ["providers", "upstreams", "routes", "plugins"] {
            let document = yaml_document(API_VERSION, 1024, 1000);
            let without = document
                .lines()
                .filter(|line| !line.starts_with(section))
                .collect::<Vec<_>>()
                .join("\n");
            let error = Document::parse(without.as_bytes(), ParseLimits::new(4096))
                .expect_err("a missing section must not be read as an empty one");
            assert!(
                matches!(error, DocumentError::Yaml { .. }),
                "expected Yaml error for missing {section}, got {error:?}"
            );
        }
    }

    #[test]
    fn parse_rejects_wrong_api_version() {
        let document = yaml_document("gateway/v2", 1024, 1000);
        match Document::parse(document.as_bytes(), ParseLimits::new(4096)) {
            Err(DocumentError::WrongApiVersion { found }) => {
                assert_eq!(found, "gateway/v2");
            }
            other => panic!("expected WrongApiVersion, got {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_document_larger_than_the_limit() {
        let bytes = VALID.as_bytes();
        match Document::parse(bytes, ParseLimits::new(bytes.len() - 1)) {
            Err(DocumentError::TooLarge { size, limit }) => {
                assert_eq!(size, bytes.len());
                assert_eq!(limit, bytes.len() - 1);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_non_utf8_bytes() {
        let bytes = [b'a', 0xff, b'b'];
        let error = Document::parse(&bytes, ParseLimits::new(16)).expect_err("not utf-8");
        assert!(
            matches!(error, DocumentError::NotUtf8),
            "expected NotUtf8, got {error:?}"
        );
        assert_eq!(error.category(), ErrorCategory::Client);
    }

    #[test]
    fn parse_accepts_a_zero_request_body_limit() {
        let document = yaml_document(API_VERSION, 0, 1000);
        let parsed = Document::parse(document.as_bytes(), ParseLimits::new(4096))
            .expect("0 means: accept no request body");
        assert_eq!(parsed.request().max_body_bytes(), 0);
    }

    #[test]
    fn parse_accepts_the_largest_request_body_limit() {
        let document = yaml_document(API_VERSION, u32::MAX, 1000);
        let parsed = Document::parse(document.as_bytes(), ParseLimits::new(4096))
            .expect("u32 is the only boundary of max_body_bytes");
        assert_eq!(parsed.request().max_body_bytes(), u32::MAX);
    }

    #[test]
    fn parse_rejects_timeout_above_ten_minutes() {
        let document = yaml_document(API_VERSION, 1024, 600_001);
        match Document::parse(document.as_bytes(), ParseLimits::new(4096)) {
            Err(DocumentError::LimitOutOfRange { field, value }) => {
                assert_eq!(field, "request.timeout_ms");
                assert_eq!(value, 600_001);
            }
            other => panic!("expected LimitOutOfRange, got {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_an_empty_document() {
        let error = Document::parse(b"", ParseLimits::new(16)).expect_err("empty document");
        assert!(
            matches!(error, DocumentError::Yaml { .. }),
            "expected Yaml error, got {error:?}"
        );
    }

    #[test]
    fn parse_rejects_a_missing_request_section() {
        let error = Document::parse(b"apiVersion: gateway/v1\n", ParseLimits::new(4096))
            .expect_err("missing request bounds");
        assert!(
            matches!(error, DocumentError::Yaml { .. }),
            "expected Yaml error, got {error:?}"
        );
    }

    #[test]
    fn parse_never_returns_more_routes_than_the_bound_allows() {
        let routes = concat!(
            "  - id: api-public-v1\n",
            "    path_prefix: /api/\n",
            "    upstream: api-backend\n",
            "  - id: api-public-v2\n",
            "    path_prefix: /api/v2/\n",
            "    upstream: api-backend\n",
            "  - id: api-public-v3\n",
            "    path_prefix: /api/v3/\n",
            "    upstream: api-backend\n",
        );
        let document = crate::config::test_support::document(NO_ENTRIES, UPSTREAM, routes);
        let limits = ParseLimits::new(16 * 1024).with_schema(
            SchemaBounds::default()
                .with(Bound::Routes, 8)
                .expect("8 is legal"),
        );
        let parsed = Document::parse(document.as_bytes(), limits)
            .expect("three routes fit a bound of eight");

        assert_eq!(parsed.routes().len(), 3);
        assert!(
            parsed.routes().len() <= limits.schema().get(Bound::Routes),
            "parse must never return more routes than the bound allows"
        );
    }
}

mod attempts;
mod bounds;
mod llm;
mod plugins;

mod providers;
mod rewrite;
mod route_pools;
