//! The research's local-provider example is a real document, not prose.
//!
//! The claim in `research/findings.md` §3.1 is that a local inference server needs no new provider
//! kind — `kind: openai` + a `base_url` + a `model_map` suffice. This test proves the *document side*
//! of that claim: the shipped example parses and publishes through the same pipeline as any other
//! provider route. Whether a particular server speaks OpenAI-compatible wire is a separate, external
//! assumption that this test deliberately does not assert.

use std::path::Path;

use nico_core::{Document, ParseLimits, RouteTarget};

fn example() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.trellis/tasks/11-13-heterogeneous-provider-research/research/local-provider-example.yaml")
}

#[test]
fn the_local_provider_example_parses() {
    let bytes = std::fs::read(example()).expect("the research example exists");
    let document = Document::parse(&bytes, ParseLimits::new(1 << 20))
        .expect("a local server is expressible with the existing provider shape");

    assert_eq!(document.providers().len(), 1);
    assert_eq!(document.routes().len(), 1);
}

#[test]
fn the_example_uses_the_existing_openai_kind() {
    let bytes = std::fs::read(example()).expect("the research example exists");
    let document = Document::parse(&bytes, ParseLimits::new(1 << 20)).expect("the example parses");

    let provider = &document.providers()[0];
    assert_eq!(provider.kind(), "openai");
    assert_eq!(
        provider.model_map().get("fast").map(String::as_str),
        Some("llama3.2:1b"),
        "the logical name resolves to the local model"
    );

    let route = &document.routes()[0];
    assert!(
        matches!(route.target(), RouteTarget::Provider(name) if name == "local-llm"),
        "the route points at the local provider"
    );
    assert_eq!(route.protocol(), Some("openai.chat"));
}

#[test]
fn the_example_needs_a_credential_ref_even_without_a_key() {
    // `credential_ref` is mandatory in the schema (raw.rs:56, semantic.rs:157). A local server has
    // no key, so the example uses a placeholder env ref. This pins that fact so a reader does not
    // "fix" the example by deleting the field and then wonder why publication refuses it.
    let bytes = std::fs::read(example()).expect("the research example exists");
    let document = Document::parse(&bytes, ParseLimits::new(1 << 20)).expect("the example parses");

    assert_eq!(
        document.providers()[0].credential_ref(),
        "env:NICO_LOCAL_UNUSED"
    );
}
