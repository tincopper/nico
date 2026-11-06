//! Rule tests for [`super::validate`].

use super::*;
use crate::{
    config::{
        bounds::ParseLimits,
        document::Document,
        route::RouteTarget,
        test_support::{
            NO_ENTRIES, PLUGIN, PROVIDER, SECOND_PLUGIN, UPSTREAM, document, document_with_plugins,
        },
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
                "  - id: llm-chat\n",
                "    path_prefix: /v1/chat/\n",
                "    provider: openai-primary\n",
                "    protocol: openai.chat\n",
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

    #[test]
    fn a_provider_route_without_a_protocol_is_rejected() {
        // Without a protocol there is no codec to decode the caller's body, so the route cannot be
        // served: the published plan would have to guess, and guessing is how a request is silently
        // forwarded as bytes the provider never asked for.
        let document = document(
            PROVIDER,
            NO_ENTRIES,
            concat!(
                "  - id: llm-chat\n",
                "    path_prefix: /v1/chat/\n",
                "    provider: openai-primary\n",
            ),
        );
        let error = parse(&document).expect_err("a provider route must name its protocol");
        assert!(
            matches!(
                error,
                DocumentError::InvalidField {
                    field: "routes[].protocol",
                    ..
                }
            ),
            "expected InvalidField for routes[].protocol, got {error:?}"
        );
        assert_eq!(error.category(), ErrorCategory::Client);
    }

    #[test]
    fn an_upstream_route_with_a_protocol_is_rejected() {
        let routes = concat!(
            "  - id: api-public-v1\n",
            "    path_prefix: /api/\n",
            "    upstream: api-backend\n",
            "    protocol: openai.chat\n",
        );
        assert_only_on_a_provider_route("routes[].protocol", routes);
    }

    #[test]
    fn an_upstream_route_with_a_mode_is_rejected() {
        let routes = concat!(
            "  - id: api-public-v1\n",
            "    path_prefix: /api/\n",
            "    upstream: api-backend\n",
            "    mode: convert\n",
        );
        assert_only_on_a_provider_route("routes[].mode", routes);
    }

    #[test]
    fn an_upstream_route_with_a_response_bound_is_rejected() {
        let routes = concat!(
            "  - id: api-public-v1\n",
            "    path_prefix: /api/\n",
            "    upstream: api-backend\n",
            "    max_response_bytes: 1024\n",
        );
        assert_only_on_a_provider_route("routes[].max_response_bytes", routes);
    }

    /// Asserts that `routes` is refused because `field` belongs to a provider route.
    ///
    /// An upstream route would otherwise read the value as a default and drop what the operator
    /// wrote, which is the failure this rule exists to prevent.
    fn assert_only_on_a_provider_route(field: &str, routes: &str) {
        let document = document(NO_ENTRIES, UPSTREAM, routes);
        let error = parse(&document).expect_err("the route has no exchange to configure");
        match error {
            DocumentError::InvalidField {
                field: named,
                reason,
            } => {
                assert_eq!(named, field);
                assert!(
                    reason.contains("only valid on a provider route"),
                    "the message must say which kind owns the field, got {reason:?}"
                );
            }
            other => panic!("expected InvalidField for {field}, got {other:?}"),
        }
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
                "  - id: llm-chat\n",
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

mod plugin_references {
    use super::*;

    /// A route that forwards to the fixture upstream and lists `plugins`.
    fn route_with_plugins(id: &str, path_prefix: &str, plugins: &str) -> String {
        format!(
            concat!(
                "  - id: {}\n",
                "    path_prefix: {}\n",
                "    upstream: api-backend\n",
                "    plugins: [{}]\n",
            ),
            id, path_prefix, plugins
        )
    }

    #[test]
    fn a_duplicate_plugin_name_is_rejected() {
        let plugins = format!("{PLUGIN}{PLUGIN}");
        let document = document_with_plugins(NO_ENTRIES, NO_ENTRIES, NO_ENTRIES, &plugins);
        let error = parse(&document).expect_err("one plugin name cannot name two entries");

        assert!(
            matches!(
                error,
                DocumentError::DuplicateId {
                    kind: "plugin",
                    ref id
                } if id == "public-jwt"
            ),
            "expected DuplicateId for the plugin, got {error:?}"
        );
        assert_eq!(error.category(), ErrorCategory::Client);
    }

    #[test]
    fn a_route_referencing_an_unknown_plugin_is_rejected() {
        let routes = route_with_plugins("api-public-v1", "/api/", "missing-plugin");
        let document = document_with_plugins(NO_ENTRIES, UPSTREAM, &routes, PLUGIN);
        let error = parse(&document).expect_err("the route names a plugin nobody declares");

        assert!(
            matches!(
                error,
                DocumentError::ReferenceUnknown {
                    kind: "plugin",
                    ref name
                } if name == "missing-plugin"
            ),
            "expected ReferenceUnknown for the plugin, got {error:?}"
        );
        assert_eq!(error.category(), ErrorCategory::Client);
    }

    #[test]
    fn a_route_referencing_the_same_plugin_twice_is_rejected() {
        let routes = route_with_plugins("api-public-v1", "/api/", "public-jwt, public-jwt");
        let document = document_with_plugins(NO_ENTRIES, UPSTREAM, &routes, PLUGIN);
        let error = parse(&document).expect_err("a plugin cannot run twice on one route");

        assert!(
            matches!(
                error,
                DocumentError::DuplicateId {
                    kind: "route-plugin",
                    ref id
                } if id == "public-jwt"
            ),
            "expected DuplicateId for the route's plugin list, got {error:?}"
        );
    }

    #[test]
    fn a_route_resolves_its_plugins_in_document_order() {
        let routes = route_with_plugins("api-public-v1", "/api/", "api-quota, public-jwt");
        let document = parse(&document_with_plugins(
            NO_ENTRIES,
            UPSTREAM,
            &routes,
            &format!("{PLUGIN}{SECOND_PLUGIN}"),
        ))
        .expect("both plugin references resolve");

        assert_eq!(
            document.routes()[0].plugins(),
            ["api-quota".to_owned(), "public-jwt".to_owned()],
            "the document's order is kept; execution order is the data plane's decision"
        );
        assert_eq!(document.plugins().len(), 2);
    }

    #[test]
    fn a_route_without_plugins_carries_none() {
        let routes = concat!(
            "  - id: api-public-v1\n",
            "    path_prefix: /api/\n",
            "    upstream: api-backend\n",
        );
        let document = parse(&document_with_plugins(NO_ENTRIES, UPSTREAM, routes, PLUGIN))
            .expect("the route needs no plugin");

        assert!(document.routes()[0].plugins().is_empty());
    }
}
