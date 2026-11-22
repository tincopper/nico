//! Tests for a traffic route's `rewrite_prefix`: the value constraints, and the target kinds it
//! belongs to.
//!
//! The forward reassembles the request target from this value and the client path's remainder, so
//! the character rules exist to make that reassembly unfailable — a query, fragment or whitespace
//! byte in the replacement would be a target the client never wrote.

use super::*;
use crate::config::test_support::{PROVIDER, SECOND_UPSTREAM, UPSTREAM, document};

/// Parses a document holding the fixture upstreams and one traffic route with `extra` route fields.
fn parse_upstream_route(extra: &str) -> Result<Document, DocumentError> {
    let upstreams = format!("{UPSTREAM}{SECOND_UPSTREAM}");
    let route = format!(
        concat!(
            "  - id: api\n",
            "    path_prefix: /api/\n",
            "    upstream: api-backend\n",
            "{}",
        ),
        extra
    );
    let yaml = document(NO_ENTRIES, &upstreams, &route);
    Document::parse(yaml.as_bytes(), ParseLimits::new(16 * 1024))
}

#[test]
fn a_rewrite_prefix_is_carried_and_replaces_the_matched_prefix() {
    let document =
        parse_upstream_route("    rewrite_prefix: /internal/\n").expect("a bare path is valid");
    let route = &document.routes()[0];
    assert_eq!(route.rewrite_prefix(), Some("/internal/"));
    assert_eq!(
        route.target(),
        &RouteTarget::Upstream("api-backend".to_owned()),
        "the rewrite is a forward fact; the match still reads the client path"
    );
}

#[test]
fn an_empty_rewrite_prefix_is_refused() {
    // The replacement is a literal prefix swap: an empty value would turn `/api/items` into
    // `items`, a target that is not an origin-form request path. Stripping the whole prefix is
    // `rewrite_prefix: /`.
    let error = parse_upstream_route("    rewrite_prefix: \"\"\n")
        .expect_err("an empty replacement is not an origin-form request path");
    assert!(matches!(
        error,
        DocumentError::InvalidField {
            field: "routes[].rewrite_prefix",
            ..
        }
    ));
}

#[test]
fn a_rewrite_prefix_must_start_with_a_slash() {
    let error = parse_upstream_route("    rewrite_prefix: internal\n")
        .expect_err("a relative value is not a request path");
    assert!(matches!(
        error,
        DocumentError::InvalidField {
            field: "routes[].rewrite_prefix",
            ..
        }
    ));
}

#[test]
fn a_rewrite_prefix_cannot_carry_a_query_or_fragment() {
    for value in ["/internal/?a=1", "/internal/#frag"] {
        let extra = format!("    rewrite_prefix: {value}\n");
        let error = parse_upstream_route(&extra)
            .expect_err("the forward reassembles the target, so these bytes are refused");
        assert!(matches!(
            error,
            DocumentError::InvalidField {
                field: "routes[].rewrite_prefix",
                ..
            }
        ));
    }
}

#[test]
fn a_rewrite_prefix_cannot_carry_whitespace() {
    let error = parse_upstream_route("    rewrite_prefix: /internal/ items\n")
        .expect_err("whitespace in a request path is a target the client never wrote");
    assert!(matches!(
        error,
        DocumentError::InvalidField {
            field: "routes[].rewrite_prefix",
            ..
        }
    ));
}

#[test]
fn a_converting_route_cannot_rewrite_its_path() {
    let yaml = document(
        PROVIDER,
        NO_ENTRIES,
        concat!(
            "  - id: llm-chat\n",
            "    path_prefix: /v1/chat/\n",
            "    provider: openai-primary\n",
            "    protocol: openai.chat\n",
            "    rewrite_prefix: /v2/chat/\n",
        ),
    );
    let error = Document::parse(yaml.as_bytes(), ParseLimits::new(16 * 1024))
        .expect_err("a converting route's path belongs to its adapter");
    assert!(
        matches!(
            error,
            DocumentError::InvalidField {
                field: "routes[].rewrite_prefix",
                ..
            }
        ),
        "expected InvalidField for the rewrite on a converting route, got {error:?}"
    );
}
