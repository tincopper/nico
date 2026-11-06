//! Rule tests for `routes[].protocol`.
//!
//! Only the shape is a document rule. Which protocol ids exist is a publication decision of the
//! crate that owns the codec registry, so these tests deliberately use a name no registry knows:
//! the point is that the document model does not track vendors.

use super::*;

/// Parses a document with one provider route whose `protocol` is the YAML `scalar` written after it.
fn with_protocol(scalar: &str) -> Result<Document, DocumentError> {
    let routes = format!(
        concat!(
            "  - id: llm-chat\n",
            "    path_prefix: /v1/chat/\n",
            "    provider: openai-primary\n",
            "    protocol: {}\n",
        ),
        scalar
    );
    parse(&document(PROVIDER, NO_ENTRIES, &routes))
}

/// Asserts that `scalar` is refused as a protocol id.
fn assert_rejected(scalar: &str) {
    let error = with_protocol(scalar).expect_err("not a protocol id");
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
fn a_dotted_name_is_a_protocol_id() {
    let document = with_protocol("other.surface").expect("the shape is the whole rule");
    assert_eq!(
        document.routes()[0].protocol(),
        Some("other.surface"),
        "an id no codec knows is still a well-formed id: the registry refuses it, not the model"
    );
}

#[test]
fn punctuation_and_case_are_accepted() {
    // A protocol id is a registry key, not a DNS name: no kebab-case rule and no lowercase rule is
    // imposed here, because the registry that will look it up has not been asked yet.
    let document = with_protocol("Vendor_V2.chat/completions").expect("printable ASCII");
    assert_eq!(
        document.routes()[0].protocol(),
        Some("Vendor_V2.chat/completions")
    );
}

#[test]
fn an_empty_protocol_is_rejected() {
    assert_rejected("\"\"");
}

#[test]
fn whitespace_in_a_protocol_is_rejected() {
    // A space would become a lookup key no registry has while looking like a valid entry in a log.
    assert_rejected("\"a b\"");
}

#[test]
fn a_control_character_in_a_protocol_is_rejected() {
    // The YAML escape becomes a real tab; it would split the log field the id is written into.
    assert_rejected("\"a\\tb\"");
}

#[test]
fn the_length_bound_is_sixty_four_bytes() {
    let at_bound = "a".repeat(64);
    let document = with_protocol(&at_bound).expect("64 bytes is the boundary, not past it");
    assert_eq!(document.routes()[0].protocol(), Some(at_bound.as_str()));

    assert_rejected(&"a".repeat(65));
}
