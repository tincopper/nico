//! What one document compiles into, and what refuses it.
//!
//! Every test compiles a real document with the real registries: the plan's job is to pair the values a
//! deployment actually assembles, so a mock codec or adapter would only assert the mocks. The two
//! exceptions are deliberate — a resolver that answers from a map (reading the environment is `unsafe`
//! in the 2024 edition and shared with every test in the binary) and an adapter that reports a native
//! protocol no codec speaks, which is the only way to reach the passthrough mismatch with one real
//! protocol in the build.

use nico_core::{ErrorCategory, Scheme};
use nico_http::llm::LlmRoutes;
use nico_llm::{
    CanonicalError, Capabilities, ChatRequest, ChatResponse, Endpoint, ExchangeBuildError,
    ProtocolId, Provider, ProviderError, ProviderKind, ProviderRegistry, ProviderRoute,
    ProviderStream, ResponseFacts, provider::OpenAiProvider,
};

use super::*;

mod facts;

#[test]
fn a_provider_route_becomes_one_published_front_end() {
    let plan = compile(&with_provider(CONVERT)).expect("the fixture compiles");

    assert_eq!(plan.len(), 1);
    assert_eq!(plan.route_ids().collect::<Vec<_>>(), ["llm"]);
    let route = plan.route("llm").expect("the route is published");
    assert!(route.converting().is_some());
    assert_eq!(route.mode_name(), "convert");
    assert_eq!(route.protocol(), "openai.chat");
    assert_eq!(route.provider(), "openai");
    assert_eq!(
        route.path(),
        "/v1/chat/completions",
        "the endpoint is the adapter's path for the configured base URL"
    );
    let destination = route.destination();
    assert_eq!(destination.host(), "127.0.0.1");
    assert_eq!(destination.port(), 8080);
    assert_eq!(destination.scheme(), Scheme::Http);
    assert_eq!(
        route.headers(),
        [
            ("content-type".to_owned(), "application/json".to_owned()),
            (
                "accept".to_owned(),
                "application/json, text/event-stream".to_owned()
            ),
            ("authorization".to_owned(), "Bearer sk-primary".to_owned()),
        ],
        "the materialized credential is presented, and nothing else is added"
    );
    assert_eq!(
        format!("{plan:?}"),
        "LlmPlan { routes: 1 }",
        "a published plan reports how many routes it covers and nothing about them"
    );
}

#[test]
fn the_rejection_bounds_are_the_canonical_errors() {
    // A rejection's code and message come from the canonical error, and `nico-http` cannot import the
    // bounds because the dependency edge points the other way. They are the same numbers by agreement
    // only, so this is the agreement: raise one without the other and a canonical code that is legal
    // becomes a `500` from a route that could have answered `4xx`.
    assert_eq!(
        nico_http::error::MAX_REJECTION_CODE_BYTES,
        nico_llm::MAX_CANONICAL_CODE_BYTES
    );
    assert_eq!(
        nico_http::error::MAX_REJECTION_MESSAGE_BYTES,
        nico_llm::MAX_CANONICAL_MESSAGE_BYTES
    );
}

#[test]
fn a_passthrough_route_is_published_without_an_exchange() {
    let plan = compile(&with_provider(PASSTHROUGH)).expect("the fixture compiles");
    let route = plan.route("llm").expect("the route is published");

    assert!(
        route.converting().is_none(),
        "the mode is the document's, not the default"
    );
    assert_eq!(route.mode_name(), "passthrough");
    assert_eq!(route.path(), "/v1/chat/completions");
    assert_eq!(route.destination().port(), 8080);
}

#[test]
fn a_passthrough_route_is_refused_when_the_two_sides_speak_different_protocols() {
    // The adapter keeps every OpenAI behaviour and reports another native protocol, which is the only
    // way one real codec can fail this check: forwarding the client's bytes would hand them to a peer
    // that parses something else.
    struct ForeignNative(OpenAiProvider);

    impl Provider for ForeignNative {
        fn kind(&self) -> ProviderKind {
            self.0.kind()
        }

        fn native_protocol(&self) -> ProtocolId {
            ProtocolId::new("vendor.responses")
        }

        fn capabilities(&self) -> Capabilities {
            self.0.capabilities()
        }

        fn endpoint(&self, route: &ProviderRoute) -> Result<Endpoint, ProviderError> {
            self.0.endpoint(route)
        }

        fn encode(
            &self,
            route: &ProviderRoute,
            request: &ChatRequest,
        ) -> Result<Vec<u8>, ProviderError> {
            self.0.encode(route, request)
        }

        fn decode_response(
            &self,
            head: &ResponseFacts,
            body: &[u8],
        ) -> Result<ChatResponse, ProviderError> {
            self.0.decode_response(head, body)
        }

        fn error_for_status(&self, status: u16) -> CanonicalError {
            self.0.error_for_status(status)
        }

        fn stream_decoder(&self) -> Box<dyn ProviderStream> {
            self.0.stream_decoder()
        }
    }

    let (protocols, _builtin) = registries();
    let mut providers = ProviderRegistry::new();
    providers
        .register(Arc::new(ForeignNative(OpenAiProvider::new())))
        .expect("one registration");
    let credentials = MapCredentials::new(&[("env:PRIMARY_KEY", "sk-primary")]);

    let error = LlmPlan::compile(
        &with_provider(PASSTHROUGH),
        &protocols,
        &providers,
        &PublishedChains::empty(),
        credentials.as_ref(),
    )
    .expect_err("a passthrough pair that disagrees is refused");

    assert!(
        matches!(
            error,
            LlmBuildError::Exchange {
                source: ExchangeBuildError::NativeProtocolMismatch { .. },
                ..
            }
        ),
        "expected NativeProtocolMismatch, got {error:?}"
    );
    assert!(
        error.to_string().contains("`llm`"),
        "the refusal names the route: {error}"
    );
    assert_eq!(error.category(), ErrorCategory::Client);
}

#[test]
fn a_route_naming_an_unknown_protocol_is_refused() {
    let document = with_provider(
        "\
routes:
  - id: llm
    path_prefix: /v1/
    provider: primary
    protocol: openai.assistants
",
    );

    let error = compile(&document).expect_err("no codec answers this protocol");
    assert!(
        matches!(
            &error,
            LlmBuildError::UnknownProtocol { route, protocol }
                if route == "llm" && protocol == "openai.assistants"
        ),
        "expected UnknownProtocol, got {error:?}"
    );
    assert_eq!(error.category(), ErrorCategory::Client);
}

#[test]
fn a_route_whose_provider_kind_has_no_adapter_is_refused() {
    let (protocols, _builtin) = registries();
    let credentials = MapCredentials::new(&[("env:PRIMARY_KEY", "sk-primary")]);

    let error = LlmPlan::compile(
        &with_provider(CONVERT),
        &protocols,
        &ProviderRegistry::new(),
        &PublishedChains::empty(),
        credentials.as_ref(),
    )
    .expect_err("no adapter answers this kind");

    assert!(
        matches!(
            &error,
            LlmBuildError::UnknownProviderKind { route, provider, kind }
                if route == "llm" && provider == "primary" && kind == "openai"
        ),
        "expected UnknownProviderKind, got {error:?}"
    );
}

#[test]
fn a_credential_that_cannot_be_read_refuses_the_document() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let reference = format!("file:{}", directory.path().join("absent").display());
    let document = document(&format!(
        "\
providers:
  - name: primary
    kind: openai
    base_url: http://127.0.0.1:8080
    credential_ref: {reference}
plugins: []
upstreams: []
{CONVERT}"
    ));
    let (protocols, providers) = registries();

    let error = LlmPlan::compile(
        &document,
        &protocols,
        &providers,
        &PublishedChains::empty(),
        &EnvFileCredentials::new(),
    )
    .expect_err("an unreadable credential refuses the document");

    assert!(
        matches!(
            &error,
            LlmBuildError::Credential { route, provider, .. }
                if route == "llm" && provider == "primary"
        ),
        "expected Credential, got {error:?}"
    );
    assert_eq!(
        error.category(),
        ErrorCategory::Upstream,
        "a missing dependency is not a document the operator spelled wrong"
    );
    assert!(
        error.to_string().contains(&reference),
        "the reference is reported so the operator can find the entry: {error}"
    );
    assert!(
        !error.to_string().contains("sk-"),
        "no value is ever quoted: {error}"
    );
}

#[test]
fn two_routes_of_one_provider_materialize_its_credential_once() {
    let document = with_provider(
        "\
routes:
  - id: llm-a
    path_prefix: /a/
    provider: primary
    protocol: openai.chat
  - id: llm-b
    path_prefix: /b/
    provider: primary
    protocol: openai.chat
",
    );
    let (protocols, providers) = registries();
    let credentials = MapCredentials::new(&[("env:PRIMARY_KEY", "sk-primary")]);

    let plan = LlmPlan::compile(
        &document,
        &protocols,
        &providers,
        &PublishedChains::empty(),
        credentials.as_ref(),
    )
    .expect("both routes compile");

    assert_eq!(plan.len(), 2);
    assert_eq!(plan.route_ids().collect::<Vec<_>>(), ["llm-a", "llm-b"]);
    assert_eq!(
        credentials.reads(),
        1,
        "a file reference is not re-opened per route"
    );
}

#[test]
fn a_route_that_runs_only_a_request_stage_is_accepted() {
    let document = with_provider(CONVERT);
    let published = chains(
        PluginChain::empty(),
        &[("llm", chain("authenticate", PluginPhases::REQUEST))],
        &[],
    );

    let plan = compile_with(&document, &published).expect("authentication is a header decision");
    assert_eq!(plan.len(), 1);
}

#[test]
fn a_route_that_runs_a_request_body_stage_is_refused() {
    let document = with_provider(CONVERT);
    let published = chains(
        PluginChain::empty(),
        &[("llm", chain("rewrite", PluginPhases::REQUEST_BODY))],
        &[],
    );

    let error = compile_with(&document, &published).expect_err("a body stage has no document");
    assert!(
        matches!(
            &error,
            LlmBuildError::RoutePluginPhase { route, plugin, phase }
                if route == "llm" && plugin == "rewrite" && *phase == "request_body"
        ),
        "expected RoutePluginPhase, got {error:?}"
    );
}

#[test]
fn a_route_that_runs_a_response_stage_is_refused() {
    for (phases, name) in [
        (PluginPhases::RESPONSE, "response"),
        (PluginPhases::RESPONSE_BODY, "response_body"),
    ] {
        let document = with_provider(CONVERT);
        let published = chains(
            PluginChain::empty(),
            &[("llm", chain("inspect", phases))],
            &[],
        );

        let error =
            compile_with(&document, &published).expect_err("a response stage has no answer");
        assert!(
            matches!(
                &error,
                LlmBuildError::RoutePluginPhase { route, plugin, phase }
                    if route == "llm" && plugin == "inspect" && *phase == name
            ),
            "expected RoutePluginPhase for `{name}`, got {error:?}"
        );
    }
}

#[test]
fn a_route_a_plugin_buffers_is_refused_because_it_already_converts_a_body() {
    let document = with_provider(CONVERT);
    let published = chains(
        PluginChain::empty(),
        &[("llm", chain("upload", PluginPhases::REQUEST))],
        &["llm"],
    );

    let error = compile_with(&document, &published).expect_err("two buffering contracts");
    assert!(
        matches!(&error, LlmBuildError::BufferedRoute { route } if route == "llm"),
        "expected BufferedRoute, got {error:?}"
    );
}

#[test]
fn a_global_body_stage_is_refused_when_the_document_converts_a_body() {
    let document = with_provider(CONVERT);
    let published = chains(
        chain("global-rewrite", PluginPhases::RESPONSE_BODY),
        &[],
        &[],
    );

    let error = compile_with(&document, &published).expect_err("the global chain runs everywhere");
    assert!(
        matches!(
            &error,
            LlmBuildError::GlobalPluginPhase { plugin, phase }
                if plugin == "global-rewrite" && *phase == "response_body"
        ),
        "expected GlobalPluginPhase, got {error:?}"
    );
}

#[test]
fn a_provider_no_route_references_is_not_read() {
    // The fixture resolver has no value for this reference and refuses every reference it is asked
    // about, so a plan that materialized the entry would fail here. Listing an endpoint before the
    // route that will use it must not be able to fail a publication it takes no part in.
    let document = document(
        "\
providers:
  - name: staged
    kind: openai
    base_url: http://127.0.0.1:8080
    credential_ref: env:NOT_CONFIGURED
plugins: []
upstreams:
  - name: api
    scheme: http
    host: 127.0.0.1
    port: 8080
routes:
  - id: api
    path_prefix: /api/
    upstream: api
",
    );

    let plan = compile(&document).expect("an unreferenced provider is not materialized");
    assert_eq!(plan.len(), 0, "an upstream document publishes no front-end");
}

#[test]
fn a_document_without_provider_routes_ignores_every_plugin_stage() {
    let document = document(
        "\
providers: []
plugins: []
upstreams:
  - name: api
    scheme: http
    host: 127.0.0.1
    port: 8080
routes:
  - id: api
    path_prefix: /api/
    upstream: api
",
    );
    let published = chains(
        chain("global-rewrite", PluginPhases::REQUEST_BODY),
        &[("api", chain("route-rewrite", PluginPhases::RESPONSE_BODY))],
        &["api"],
    );

    let plan = compile_with(&document, &published)
        .expect("no exchange means no body ownership to disagree about");
    assert_eq!(plan.len(), 0, "an upstream document publishes no front-end");
    assert_eq!(plan.route_ids().count(), 0);
}
