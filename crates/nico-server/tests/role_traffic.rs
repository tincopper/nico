//! What the traffic role does with a document that asks for an AI route.
//!
//! The role is a property of the build, so this file only compiles in the traffic-only form
//! (`cargo test -p nico-server --no-default-features --features traffic`). Its whole purpose is to
//! pin the one behavior that must not depend on a runtime check: a build without the AI crate
//! refuses the document instead of publishing routes it has no codecs for.

// The whole file describes the traffic-only build; with the `llm` feature on there is no
// `NoLlmCompiler` to test, and the scenarios belong to that build's own suite.
#![cfg(not(feature = "llm"))]
#![expect(
    clippy::expect_used,
    reason = "a test that cannot build its fixture has nothing useful to return; the helper's `panic!` sits in the fixture builder below"
)]

use std::sync::Arc;

use nico_core::{Document, ErrorCategory, ParseLimits};
use nico_http::PublishedChains;
use nico_server::reload::{LlmCompiler, NoLlmCompiler};

/// Parses `routes` around a provider and an upstream.
fn document(routes: &str) -> Document {
    // A raw literal: a `\`-continued one strips the indentation that makes the fields children of
    // their sections.
    let text = format!(
        r#"apiVersion: gateway/v1
request:
  max_body_bytes: 1048576
  timeout_ms: 30000
plugins: []
providers:
  - name: primary
    kind: openai
    base_url: http://127.0.0.1:8080
    credential_ref: env:PRIMARY_KEY
upstreams:
  - name: api-backend
    scheme: http
    host: 127.0.0.1
    port: 8080
routes:
{routes}"#
    );
    Document::parse(text.as_bytes(), ParseLimits::new(64 * 1024)).expect("the fixture parses")
}

#[test]
fn a_traffic_build_compiles_a_document_with_no_provider_route() {
    let document = document(
        r#"  - id: api-public
    path_prefix: /api/
    upstream: api-backend
"#,
    );
    let plan = NoLlmCompiler
        .compile(&document, &PublishedChains::empty())
        .expect("a traffic document is served");
    assert_eq!(
        plan.route_count(),
        0,
        "a traffic build publishes no front-end"
    );
}

#[test]
fn a_traffic_build_refuses_a_document_with_a_provider_route() {
    // The alternative — publishing the route with an empty plan — would answer every AI request
    // with "no provider configured" until an operator noticed. A refused document keeps the last
    // known good generation serving, which is the same answer every other publication failure gets.
    let document = document(
        r#"  - id: llm-chat
    path_prefix: /v1/chat/
    provider: primary
    protocol: openai.chat
"#,
    );
    let error = match NoLlmCompiler.compile(&document, &PublishedChains::empty()) {
        Err(error) => error,
        Ok(plan) => panic!(
            "a traffic build cannot serve an AI route, but it published {}",
            plan.route_count()
        ),
    };
    assert_eq!(
        error.category(),
        ErrorCategory::Client,
        "the document asks for a role this build does not have"
    );
    let message = error.to_string();
    assert!(
        message.contains("traffic only"),
        "the refusal names the role: {message}"
    );
    assert!(
        !message.contains("PRIMARY_KEY") && !message.contains("api.openai.com"),
        "the refusal quotes no credential or endpoint: {message}"
    );
}

#[test]
fn the_traffic_plan_serves_no_route_at_all() {
    let plan = NoLlmCompiler
        .compile(&document("  []"), &PublishedChains::empty())
        .expect("an empty document is served");
    let routes: Arc<dyn nico_http::llm::LlmRoutes> = plan.routes();
    assert_eq!(routes.len(), 0);
    assert!(routes.is_empty());
    assert!(
        routes.route("anything").is_none(),
        "a traffic build has no front-end to hand out"
    );
}
