//! The runtime a process compiles with: the registries it was built from, and what its `Debug` says.
//!
//! The constructors are the assembly seam. `builtin` is what a process gets (and the only path that
//! reads a real `file:` credential end to end); `with_parts` is what a test or a future assembly gets;
//! `with_shared` is what ties a runtime to breaker state another generation already populated. None is
//! allowed to be a second compiler: all reach the same `LlmPlan::compile`.

use std::{
    io,
    sync::{Arc, Mutex},
};

use nico_http::llm::LlmRoutes;
use nico_llm::{
    CanonicalError, Capabilities, ChatRequest, Endpoint, ProtocolId, Provider, ProviderError,
    ProviderKind, ProviderRegistry, ProviderRoute, ProviderStream,
    provider::openai::OpenAiProvider,
};
use tracing_subscriber::fmt::MakeWriter;

use super::*;

/// A writer that keeps what a subscriber formats, so a test can read its own log lines.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("the capture lock is not poisoned")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Runs `body` under a subscriber that writes into a buffer, and returns what it wrote.
///
/// A scoped subscriber rather than a global one: the log is this test's subject, and installing a
/// process-wide subscriber would decide what every other test in the binary can log.
fn captured(body: impl FnOnce()) -> String {
    // A callsite's dispatch is registered on its first firing, under a process-wide lock that two
    // threads can race to take. Two tests that both capture `log_registries` fire those callsites
    // for the first time concurrently, and a lost race leaves the callsite cached as disabled -
    // permanently, because nothing here rebuilds the cache. Warming them once, serialized, moves
    // the registration out of the race: what the two tests then measure is only the log content.
    static WARMED: std::sync::Once = std::sync::Once::new();
    WARMED.call_once(|| {
        let (protocols, providers) = nico_llm::builtin_registry();
        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture)
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            super::super::runtime::log_registries(&protocols, &providers);
        });
    });

    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_target(false)
        .finish();
    tracing::subscriber::with_default(subscriber, body);
    let bytes = capture
        .0
        .lock()
        .expect("the capture lock is not poisoned")
        .clone();
    String::from_utf8(bytes).expect("the subscriber writes UTF-8")
}

#[test]
fn a_runtime_compiles_with_the_registries_it_was_built_from() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let path = directory.path().join("primary.key");
    std::fs::write(&path, "sk-file").expect("the credential fixture is written");
    let document = document(&format!(
        "\
providers:
  - name: primary
    kind: openai
    base_url: http://127.0.0.1:8080
    credential_ref: file:{}
plugins: []
upstreams: []
{CONVERT}",
        path.display()
    ));

    let runtime = LlmRuntime::builtin();
    let plan = runtime
        .compile(&document, &PublishedChains::empty())
        .expect("the built-in runtime serves the fixture");
    let route = plan.route("llm").expect("the route is published");

    assert_eq!(
        route.headers().last(),
        Some(&("authorization".to_owned(), "Bearer sk-file".to_owned()))
    );
    let rendered = format!("{runtime:?}");
    assert!(
        rendered.contains("openai.chat") && rendered.contains("openai"),
        "the runtime reports the kinds it serves: {rendered}"
    );
    assert!(
        !rendered.contains("sk-file"),
        "and never a credential: {rendered}"
    );
}

#[test]
fn a_runtime_built_from_parts_uses_the_registries_it_was_given() {
    let (protocols, _builtin) = registries();
    let runtime = LlmRuntime::with_parts(
        protocols,
        ProviderRegistry::new(),
        MapCredentials::new(&[("env:PRIMARY_KEY", "sk-primary")]),
    );

    let error = runtime
        .compile(&with_provider(CONVERT), &PublishedChains::empty())
        .expect_err("the empty registry serves no provider kind");
    assert!(
        matches!(error, LlmBuildError::UnknownProviderKind { .. }),
        "expected UnknownProviderKind, got {error:?}"
    );
}

#[test]
fn every_registered_side_is_reported_with_its_capabilities() {
    let (protocols, providers) = nico_llm::builtin_registry();
    let text = captured(|| super::super::runtime::log_registries(&protocols, &providers));

    assert!(
        text.contains("LLM client protocol registered"),
        "one line per codec, got: {text}"
    );
    assert!(text.contains("llm.protocol=openai.chat"), "got: {text}");
    assert!(
        text.contains("LLM provider registered"),
        "one line per adapter, got: {text}"
    );
    assert!(text.contains("llm.provider=openai"), "got: {text}");
    assert_eq!(
        text.matches("llm.capabilities=streaming,tools,images,reasoning,response_format")
            .count(),
        4,
        "the OpenAI codec and adapter, the Gemini codec and the Gemini adapter declare the same \
         five axes, and `files` is the one none of them has: {text}"
    );
    let openai = text
        .lines()
        .find(|line| line.contains("llm.provider=openai"))
        .expect("the OpenAI adapter is reported");
    assert!(
        !openai.contains("llm.version="),
        "`openai.chat` has no versioned spelling, so the field is absent rather than empty: {openai}"
    );
    let anthropic = text
        .lines()
        .find(|line| line.contains("llm.provider=anthropic"))
        .expect("the Anthropic adapter is reported");
    assert!(
        anthropic.contains("llm.version=2023-06-01"),
        "a vendor that versions reports the revision the adapter sends: {anthropic}"
    );
    assert!(
        anthropic.contains("llm.capabilities=streaming,tools,images"),
        "the adapter declares only what it maps: {anthropic}"
    );
    assert!(
        !anthropic.contains("reasoning"),
        "a thinking block cannot be written back, so the axis is not declared: {anthropic}"
    );
    let gemini = text
        .lines()
        .find(|line| line.contains("llm.provider=gemini"))
        .expect("the Gemini adapter is reported");
    assert!(
        gemini.contains("llm.version=v1beta"),
        "the revision this adapter writes into the request path is the one the assembly reports: \
         {gemini}"
    );
}

#[test]
fn a_side_that_pins_a_revision_reports_it() {
    /// The built-in adapter with a revision, which no OpenAI wire value carries: the field is only
    /// written when the side answers with one.
    struct Pinned(OpenAiProvider);

    impl Provider for Pinned {
        fn kind(&self) -> ProviderKind {
            self.0.kind()
        }

        fn native_protocol(&self) -> ProtocolId {
            self.0.native_protocol()
        }

        fn capabilities(&self) -> Capabilities {
            self.0.capabilities()
        }

        fn version(&self) -> Option<&'static str> {
            Some("2026-01-01")
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
            response: &nico_llm::ResponseFacts,
            body: &[u8],
        ) -> Result<nico_llm::ChatResponse, ProviderError> {
            self.0.decode_response(response, body)
        }

        fn error_for_status(&self, status: u16) -> CanonicalError {
            self.0.error_for_status(status)
        }

        fn stream_decoder(&self) -> Box<dyn ProviderStream> {
            self.0.stream_decoder()
        }
    }

    let mut providers = ProviderRegistry::new();
    providers
        .register(Arc::new(Pinned(OpenAiProvider::new())))
        .expect("one kind registers once");
    let (protocols, _) = nico_llm::builtin_registry();

    let text = captured(|| super::super::runtime::log_registries(&protocols, &providers));

    assert!(
        text.contains("llm.version=2026-01-01"),
        "a side that pins a revision reports it, got: {text}"
    );
}

/// The fixture provider with an explicit breaker, so a test names the identity's parameters itself.
fn provider_with_breaker(threshold: u32) -> String {
    format!(
        "\
providers:
  - name: primary
    kind: openai
    base_url: http://127.0.0.1:8080
    credential_ref: env:PRIMARY_KEY
    circuit_breaker:
      failure_threshold: {threshold}
      cooldown_ms: 30000
"
    )
}

/// A document with the fixture provider retuned to `threshold`, and the converting fixture route.
fn document_with_breaker(threshold: u32) -> Document {
    document(&format!(
        "{}plugins: []\nupstreams: []\n{CONVERT}",
        provider_with_breaker(threshold)
    ))
}

#[test]
fn a_generation_inherits_the_history_of_the_targets_it_kept_and_drops_the_rest() {
    use nico_llm::{
        Admission, AttemptOutcome, BreakerParams, BreakerPhase, ProviderKind, TargetIdentity,
    };
    use tokio::time::Instant;

    let (protocols, providers) = registries();
    let shared = Arc::new(SharedServices::new());
    let runtime = LlmRuntime::with_shared(
        protocols,
        providers,
        MapCredentials::new(&[("env:PRIMARY_KEY", "sk-primary")]),
        Arc::clone(&shared),
    );
    // The identity the first generation compiles for `primary`: the document's parameters are part of
    // it, which is why the fixture writes them instead of taking the defaults.
    let identity = TargetIdentity::new(
        "primary",
        "http://127.0.0.1:8080",
        ProviderKind::new("openai"),
        BreakerParams::new(1, 30_000),
    );
    let now = Instant::now();

    runtime
        .compile(&document_with_breaker(1), &PublishedChains::empty())
        .expect("the first generation compiles");
    shared
        .breakers()
        .record(&identity, AttemptOutcome::Failure, None, now);
    assert_eq!(
        shared.breakers().admit(&identity, now),
        Admission::Skip(BreakerPhase::Open),
        "one failure with a threshold of one opens the target"
    );

    // The same document republished: the target's identity is unchanged, so its history survives the
    // generation, which is the whole point of holding the table outside the plan.
    runtime
        .compile(&document_with_breaker(1), &PublishedChains::empty())
        .expect("the republished generation compiles");
    assert_eq!(
        shared.breakers().admit(&identity, now),
        Admission::Skip(BreakerPhase::Open),
        "an unchanged target keeps what it learned"
    );

    // A retuned threshold is a different identity: the counts described the rules the operator has
    // just replaced, so they are dropped rather than inherited under new rules.
    runtime
        .compile(&document_with_breaker(2), &PublishedChains::empty())
        .expect("the retuned generation compiles");
    assert_eq!(
        shared.breakers().admit(&identity, now),
        Admission::Admit,
        "a retuned target starts over"
    );
}
