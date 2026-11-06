//! Tests for the fields that describe an LLM exchange: `routes[].protocol`, `routes[].mode` and
//! `providers[].model_map`.
//!
//! The rejections live where the rules do: a field a target kind does not own is refused by target
//! resolution (`reference`), the shape of a protocol id and of a model-map entry is refused by
//! semantic validation, and the enum spellings are refused by serde. This module covers what a
//! document that parses carries, including the shipped example.

use std::path::Path;

use super::*;
use crate::config::test_support::{PROVIDER, document};

/// A provider route that names `protocol`, with `extra` appended as further route fields.
fn provider_route(extra: &str) -> String {
    format!(
        concat!(
            "  - id: llm-chat\n",
            "    path_prefix: /v1/chat/\n",
            "    provider: openai-primary\n",
            "    protocol: openai.chat\n",
            "{}",
        ),
        extra
    )
}

/// Parses a document holding the fixture provider and one provider route.
fn parse_provider_route(extra: &str) -> Result<Document, DocumentError> {
    let yaml = document(PROVIDER, NO_ENTRIES, &provider_route(extra));
    Document::parse(yaml.as_bytes(), ParseLimits::new(16 * 1024))
}

#[test]
fn a_provider_route_converts_unless_the_document_says_otherwise() {
    let document = parse_provider_route("").expect("fixture is valid");
    let route = &document.routes()[0];
    assert_eq!(route.protocol(), Some("openai.chat"));
    assert_eq!(route.mode(), RouteMode::Convert);
    assert_eq!(
        route.max_response_bytes(),
        None,
        "a route that writes no response bound inherits the document's"
    );
}

#[test]
fn a_provider_route_can_passthrough() {
    let document = parse_provider_route("    mode: passthrough\n").expect("fixture is valid");
    assert_eq!(document.routes()[0].mode(), RouteMode::Passthrough);
}

#[test]
fn a_provider_route_can_tighten_the_response_bound() {
    let document =
        parse_provider_route("    max_response_bytes: 1024\n").expect("fixture is valid");
    assert_eq!(document.routes()[0].max_response_bytes(), Some(1024));
}

#[test]
fn an_unknown_mode_is_a_schema_error() {
    // `RouteMode` is a closed set: an unknown spelling cannot become a value, so it is reported
    // where the document is decoded rather than as a semantic rule.
    let error = parse_provider_route("    mode: rewrite\n").expect_err("unknown mode");
    assert!(
        matches!(error, DocumentError::Yaml { .. }),
        "expected a schema error for an unknown mode, got {error:?}"
    );
}

#[test]
fn a_provider_model_map_is_preserved() {
    let providers = format!(
        concat!(
            "{}",
            "    model_map:\n",
            "      fast: small-model\n",
            "      careful: large-model\n",
        ),
        PROVIDER
    );
    let yaml = document(&providers, NO_ENTRIES, NO_ENTRIES);
    let document = Document::parse(yaml.as_bytes(), ParseLimits::new(16 * 1024))
        .expect("both mapping entries are legal names");

    let map = document.providers()[0].model_map();
    assert_eq!(map.len(), 2);
    assert_eq!(map.get("fast").map(String::as_str), Some("small-model"));
    assert_eq!(map.get("careful").map(String::as_str), Some("large-model"));
}

#[test]
fn the_shipped_example_document_carries_a_converted_chat_route() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/resources.yaml");
    let bytes = std::fs::read(&path).expect("the shipped resources document is readable");
    let document = Document::parse(&bytes, ParseLimits::new(1024 * 1024))
        .expect("the shipped resources document is valid");

    assert_eq!(document.providers().len(), 1);
    assert_eq!(document.upstreams().len(), 1);
    // Two plugin entries plus the buffered upload guard the example documents.
    assert_eq!(document.plugins().len(), 3);
    // The API, the provider route, and the buffered upload route.
    assert_eq!(document.routes().len(), 3);

    let chat = &document.routes()[1];
    assert_eq!(chat.protocol(), Some("openai.chat"));
    assert_eq!(chat.mode(), RouteMode::Convert);
    assert_eq!(chat.max_response_bytes(), Some(4 * 1024 * 1024));
    assert!(
        chat.max_response_bytes()
            .is_some_and(|bytes| bytes <= document.request().max_response_bytes()),
        "a route response bound can only tighten the document bound"
    );
    // The example maps one logical name, so the field is exercised by the document the local
    // configuration source serves.
    assert_eq!(
        document.providers()[0]
            .model_map()
            .get("fast")
            .map(String::as_str),
        Some("gpt-4o-mini")
    );
}
