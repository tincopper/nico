//! The runtime a process compiles with: the registries it was built from, and what its `Debug` says.
//!
//! The two constructors are the assembly seam. `builtin` is what a process gets (and the only path
//! that reads a real `file:` credential end to end), `with_parts` is what a test or a future assembly
//! gets. Neither is allowed to be a second compiler: both reach the same `LlmPlan::compile`.

use nico_http::llm::LlmRoutes;
use nico_llm::ProviderRegistry;

use super::*;

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
