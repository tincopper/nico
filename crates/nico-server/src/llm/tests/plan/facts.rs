//! The per-provider facts one document entry compiles into route facts.
//!
//! The document's values reach an adapter only through `ProviderRoute`, and this plan step is the
//! single place that happens. A unit test of the facts or of the enforcement would still pass if the
//! wiring were dropped here, so the seam gets its own assertions.
//!
//! Split from the route-planning scenarios by subject rather than by length: these two tests are about
//! what one provider entry compiles into, not about how a route is published.

use std::num::NonZeroU32;

use nico_core::Document;
use nico_http::llm::LlmRoutes;
use nico_llm::{Credential, ExchangeMode, ProtocolId, ProviderKind, ProviderRoute};

use super::super::{CONVERT, compile, document, with_ceiling, with_provider};

#[test]
fn a_provider_entry_compiles_its_output_ceiling_into_the_route_facts() {
    // The ceiling is endpoint policy, and this module is the only place the document's value becomes
    // route facts: a unit test of `ProviderRoute` would still pass if the wiring were dropped here.
    assert_eq!(
        compile_facts(&with_provider(CONVERT)).max_output_tokens(),
        None,
        "an entry that names no ceiling must not acquire one"
    );

    let facts = compile_facts(&with_ceiling(4096, CONVERT));
    assert_eq!(
        facts.max_output_tokens().map(NonZeroU32::get),
        Some(4096),
        "the compiled facts carry the document's value, not a gateway default"
    );
}

#[test]
fn the_operators_allowlist_reaches_the_published_front_end() {
    // This module is the only place the document's allowlist meets the compiled route facts, so the
    // seam needs its own assertion: a unit test of the enforcement would still pass if the wiring were
    // dropped here.
    let document = document(&format!(
        "\
providers:
  - name: primary
    kind: openai
    base_url: http://127.0.0.1:8080
    credential_ref: env:PRIMARY_KEY
    model_allowlist: [gpt-allowed]
plugins: []
upstreams: []
{CONVERT}"
    ));
    let plan = compile(&document).expect("the fixture compiles");
    let route = plan.route("llm").expect("the route is published");
    let converting = route
        .converting()
        .expect("a convert route has a translation half");
    let view = nico_http::llm::LlmRequest::new(
        "POST",
        "/v1/chat/completions",
        Vec::new(),
        br#"{"model":"gpt-other","messages":[{"role":"user","content":"hi"}]}"#,
    );

    let error = converting
        .prepare(&view)
        .expect_err("the route serves one model, and this is not it");

    match error {
        nico_http::error::HttpError::LlmRejected { rejection } => {
            assert_eq!(rejection.status(), 403);
            assert_eq!(rejection.code(), "model_not_allowed");
        }
        other => panic!("expected the client codec's refusal, got {other:?}"),
    }
}

/// Compiles the fixture document's provider entry into the route facts a plan would build.
///
/// `route_facts` is the plan's own step, so the assertion is about the wiring rather than about
/// `ProviderRoute`, which has its own tests in `nico-llm`.
fn compile_facts(document: &Document) -> ProviderRoute {
    super::super::super::plan::route_facts(
        "llm",
        ProtocolId::new("openai.chat"),
        ProviderKind::new("openai"),
        &document.providers()[0],
        Credential::new("sk-primary"),
        ExchangeMode::Convert,
    )
    .expect("the fixture entry forms route facts")
}
