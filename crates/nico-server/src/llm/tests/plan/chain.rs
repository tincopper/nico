//! What one route's attempt chain compiles into, and which chains refuse the publication.
//!
//! The chain compiler must prove two things per implement.md checkpoint A: a route that declares no
//! attempts compiles into exactly the shape a phase-1 publication produced (one target), and a chain
//! member that cannot be served refuses the whole route naming the member. The byte-identical
//! behavior of a one-target chain is asserted at the exchange level (`nico-llm`'s route tests) and
//! through the data plane by the e2e suites, which compile through this plan and pass unchanged.

use std::sync::Arc;

use nico_core::{Document, ErrorCategory};
use nico_http::PublishedChains;

use super::super::{
    CONVERT, MapCredentials, PASSTHROUGH, PROVIDER, compile, document, registries, with_provider,
};
use crate::llm::{LlmBuildError, LlmPlan};

/// A second provider entry: the fixture's kind, but its own name and credential reference.
fn secondary_provider() -> String {
    format!(
        "{PROVIDER}  - name: secondary\n    kind: openai\n    base_url: http://127.0.0.1:8080\n    credential_ref: env:SECONDARY_KEY\n"
    )
}

/// A second provider entry whose kind no registered adapter serves.
fn secondary_provider_with(kind: &str) -> String {
    format!(
        "{PROVIDER}  - name: secondary\n    kind: {kind}\n    base_url: http://127.0.0.1:8080\n    credential_ref: env:SECONDARY_KEY\n"
    )
}

/// Parses a document carrying `providers` and a convert route that falls back to `secondary`.
fn chain_document(providers: &str) -> Document {
    document(&format!(
        "{providers}plugins: []\nupstreams: []\n{CONVERT}    fallback: [secondary]\n"
    ))
}

/// Compiles `document` with a resolver that answers `credentials`, like a process would.
fn compile_chain(
    document: &Document,
    credentials: Arc<MapCredentials>,
) -> Result<LlmPlan, LlmBuildError> {
    let (protocols, providers) = registries();
    LlmPlan::compile(
        document,
        &protocols,
        &providers,
        &PublishedChains::empty(),
        credentials.as_ref(),
        &Arc::new(crate::llm::shared::SharedServices::new()),
    )
}

#[test]
fn a_route_without_attempts_compiles_one_target() {
    let plan = compile(&with_provider(CONVERT)).expect("the fixture compiles");
    assert_eq!(
        plan.targets("llm"),
        Some(1),
        "no fallback and no declared budget is the phase-1 shape: one target"
    );
}

#[test]
fn a_fallback_chain_compiles_every_member_and_reads_one_credential_per_provider() {
    let document = chain_document(&secondary_provider());
    let credentials = MapCredentials::new(&[
        ("env:PRIMARY_KEY", "sk-primary"),
        ("env:SECONDARY_KEY", "sk-secondary"),
    ]);
    let plan = compile_chain(&document, credentials.clone()).expect("both members serve");
    assert_eq!(
        plan.targets("llm"),
        Some(2),
        "the primary and the one declared fallback"
    );
    assert_eq!(
        credentials.reads(),
        2,
        "one materialization per provider entry, shared across members"
    );
}

#[test]
fn a_member_whose_kind_no_adapter_serves_refuses_the_publication_naming_the_member() {
    let document = chain_document(&secondary_provider_with("nosuchkind"));
    let credentials = MapCredentials::new(&[
        ("env:PRIMARY_KEY", "sk-primary"),
        ("env:SECONDARY_KEY", "sk-secondary"),
    ]);
    let error = compile_chain(&document, credentials).expect_err("the member cannot be served");
    assert!(
        matches!(
            error,
            LlmBuildError::UnknownProviderKind {
                ref provider,
                ref kind,
                ..
            } if provider == "secondary" && kind == "nosuchkind"
        ),
        "expected UnknownProviderKind naming the member, got {error:?}"
    );
    assert_eq!(error.category(), ErrorCategory::Client);
}

#[test]
fn a_member_with_an_unreadable_credential_refuses_the_publication() {
    let document = chain_document(&secondary_provider());
    let credentials = MapCredentials::new(&[("env:PRIMARY_KEY", "sk-primary")]);
    let error = compile_chain(&document, credentials).expect_err("the member cannot be served");
    assert!(
        matches!(
            error,
            LlmBuildError::Credential { ref provider, .. } if provider == "secondary"
        ),
        "expected Credential naming the member, got {error:?}"
    );
    // The category is the resolver's own classification (the fixture reports a client-side refusal);
    // the chain carries it through rather than re-deciding it.
    assert_eq!(error.category(), ErrorCategory::Client);
}

#[test]
fn a_passthrough_route_with_a_fallback_refuses_the_publication() {
    let document = document(&format!(
        "{}plugins: []\nupstreams: []\n{PASSTHROUGH}    fallback: [secondary]\n",
        secondary_provider()
    ));
    let credentials = MapCredentials::new(&[
        ("env:PRIMARY_KEY", "sk-primary"),
        ("env:SECONDARY_KEY", "sk-secondary"),
    ]);
    let error = compile_chain(&document, credentials)
        .expect_err("a passthrough route has no failure to fall back from");
    assert!(
        matches!(error, LlmBuildError::PassthroughAttempts { .. }),
        "expected PassthroughAttempts, got {error:?}"
    );
    assert_eq!(error.category(), ErrorCategory::Client);
}

#[test]
fn a_passthrough_route_with_a_retry_budget_refuses_the_publication() {
    let document = with_provider(&format!("{PASSTHROUGH}    retry_budget: 2\n"));
    let credentials = MapCredentials::new(&[("env:PRIMARY_KEY", "sk-primary")]);
    let error = compile_chain(&document, credentials)
        .expect_err("a passthrough route has no failure to retry");
    assert!(
        matches!(error, LlmBuildError::PassthroughAttempts { .. }),
        "expected PassthroughAttempts, got {error:?}"
    );
}
