//! Rule tests for [`super::validate`].

use super::*;
use crate::{
    config::{
        bounds::ParseLimits,
        document::Document,
        route::RouteTarget,
        test_support::{NO_ENTRIES, PROVIDER, UPSTREAM, document},
    },
    error::ErrorCategory,
};

fn parse(document: &str) -> Result<Document, DocumentError> {
    Document::parse(document.as_bytes(), ParseLimits::new(16 * 1024))
}

mod targets {
    use super::*;

    #[test]
    fn a_route_without_a_target_is_rejected() {
        let document = document(
            NO_ENTRIES,
            NO_ENTRIES,
            concat!("  - id: api-public-v1\n", "    path_prefix: /api/\n"),
        );
        let error = parse(&document).expect_err("missing target");
        assert!(
            matches!(
                error,
                DocumentError::RouteTargetMissing { ref id } if id == "api-public-v1"
            ),
            "expected RouteTargetMissing, got {error:?}"
        );
        assert_eq!(error.category(), ErrorCategory::Client);
    }

    #[test]
    fn a_route_with_two_targets_is_rejected() {
        let document = document(
            NO_ENTRIES,
            NO_ENTRIES,
            concat!(
                "  - id: api-public-v1\n",
                "    path_prefix: /api/\n",
                "    upstream: api-backend\n",
                "    provider: openai-primary\n",
            ),
        );
        let error = parse(&document).expect_err("two targets");
        assert!(
            matches!(
                error,
                DocumentError::RouteTargetAmbiguous { ref id } if id == "api-public-v1"
            ),
            "expected RouteTargetAmbiguous, got {error:?}"
        );
        assert_eq!(error.category(), ErrorCategory::Client);
    }

    #[test]
    fn a_route_with_one_target_resolves_it() {
        let document = document(
            PROVIDER,
            UPSTREAM,
            concat!(
                "  - id: api-public-v1\n",
                "    path_prefix: /api/\n",
                "    upstream: api-backend\n",
                "  - id: ai-chat\n",
                "    path_prefix: /v1/chat/\n",
                "    provider: openai-primary\n",
            ),
        );
        let document = parse(&document).expect("both targets resolve");
        let routes = document.routes();
        assert_eq!(
            routes[0].target(),
            &RouteTarget::Upstream("api-backend".to_owned())
        );
        assert_eq!(
            routes[1].target(),
            &RouteTarget::Provider("openai-primary".to_owned())
        );
    }
}

mod references {
    use super::*;

    #[test]
    fn an_unknown_upstream_is_rejected() {
        let document = document(
            NO_ENTRIES,
            UPSTREAM,
            concat!(
                "  - id: api-public-v1\n",
                "    path_prefix: /api/\n",
                "    upstream: missing-backend\n",
            ),
        );
        let error = parse(&document).expect_err("unknown upstream");
        assert!(
            matches!(
                error,
                DocumentError::ReferenceUnknown {
                    kind: "upstream",
                    ref name
                } if name == "missing-backend"
            ),
            "expected ReferenceUnknown for upstream, got {error:?}"
        );
        assert_eq!(error.category(), ErrorCategory::Client);
    }

    #[test]
    fn an_unknown_provider_is_rejected() {
        let document = document(
            PROVIDER,
            NO_ENTRIES,
            concat!(
                "  - id: ai-chat\n",
                "    path_prefix: /v1/chat/\n",
                "    provider: missing-provider\n",
            ),
        );
        let error = parse(&document).expect_err("unknown provider");
        assert!(
            matches!(
                error,
                DocumentError::ReferenceUnknown {
                    kind: "provider",
                    ref name
                } if name == "missing-provider"
            ),
            "expected ReferenceUnknown for provider, got {error:?}"
        );
        assert_eq!(error.category(), ErrorCategory::Client);
    }

    #[test]
    fn a_duplicate_route_id_is_rejected() {
        let document = document(
            NO_ENTRIES,
            UPSTREAM,
            concat!(
                "  - id: api-public-v1\n",
                "    path_prefix: /api/\n",
                "    upstream: api-backend\n",
                "  - id: api-public-v1\n",
                "    path_prefix: /other/\n",
                "    upstream: api-backend\n",
            ),
        );
        let error = parse(&document).expect_err("duplicate route id");
        assert!(
            matches!(
                error,
                DocumentError::DuplicateId {
                    kind: "route",
                    ref id
                } if id == "api-public-v1"
            ),
            "expected DuplicateId for route, got {error:?}"
        );
        assert_eq!(error.category(), ErrorCategory::Client);
    }

    #[test]
    fn a_duplicate_upstream_name_is_rejected() {
        let upstreams = concat!(
            "  - name: api-backend\n",
            "    scheme: http\n",
            "    host: 127.0.0.1\n",
            "    port: 8080\n",
            "  - name: api-backend\n",
            "    scheme: http\n",
            "    host: 127.0.0.1\n",
            "    port: 8081\n",
        );
        let document = document(NO_ENTRIES, upstreams, NO_ENTRIES);
        let error = parse(&document).expect_err("duplicate upstream name");
        assert!(
            matches!(
                error,
                DocumentError::DuplicateId {
                    kind: "upstream",
                    ref id
                } if id == "api-backend"
            ),
            "expected DuplicateId for upstream, got {error:?}"
        );
    }

    #[test]
    fn a_duplicate_provider_name_is_rejected() {
        let providers = concat!(
            "  - name: openai-primary\n",
            "    kind: openai\n",
            "    base_url: https://api.openai.com\n",
            "    credential_ref: env:OPENAI_API_KEY\n",
            "  - name: openai-primary\n",
            "    kind: openai\n",
            "    base_url: https://api.openai.com\n",
            "    credential_ref: env:OPENAI_API_KEY\n",
        );
        let document = document(providers, NO_ENTRIES, NO_ENTRIES);
        let error = parse(&document).expect_err("duplicate provider name");
        assert!(
            matches!(
                error,
                DocumentError::DuplicateId {
                    kind: "provider",
                    ref id
                } if id == "openai-primary"
            ),
            "expected DuplicateId for provider, got {error:?}"
        );
    }
}
