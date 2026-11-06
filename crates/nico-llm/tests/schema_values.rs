//! The validation paths of the canonical *leaf values*.
//!
//! Split from `schema_rejects.rs` (which owns the request/response tree) so each file stays inside
//! the 500-line budget and a reader knows where a given invariant lives.

use nico_core::ErrorCategory;
use nico_llm::{
    CanonicalError, ContentPart, ImageSource, ModelId, RawJson, ResponseId, SchemaError,
    ToolCallId, ToolName, UsageCounts,
};

#[test]
fn an_empty_model_is_rejected() {
    let error = ModelId::try_new("   ").expect_err("empty model must be rejected");
    assert_eq!(error, SchemaError::Empty { field: "model" });
}

#[test]
fn names_that_address_something_must_not_be_empty() {
    assert_eq!(
        ToolName::try_new("").expect_err("empty tool name"),
        SchemaError::Empty { field: "tool.name" }
    );
    assert_eq!(
        ToolCallId::try_new(" ").expect_err("empty call id"),
        SchemaError::Empty {
            field: "tool_call.id"
        }
    );
    assert_eq!(
        ResponseId::try_new("").expect_err("empty response id"),
        SchemaError::Empty {
            field: "response.id"
        }
    );
}

#[test]
fn an_image_source_must_carry_a_usable_location() {
    assert_eq!(
        ImageSource::Url(String::new()).validate(),
        Err(SchemaError::Empty { field: "image.url" })
    );
    assert!(matches!(
        ImageSource::Url("ftp://example.com/a.png".to_owned()).validate(),
        Err(SchemaError::Invalid {
            field: "image.url",
            ..
        })
    ));
    assert_eq!(
        ImageSource::Base64 {
            media_type: String::new(),
            data: vec![1],
        }
        .validate(),
        Err(SchemaError::Empty {
            field: "image.media_type"
        })
    );
    assert_eq!(
        ImageSource::Base64 {
            media_type: "image/png".to_owned(),
            data: Vec::new(),
        }
        .validate(),
        Err(SchemaError::Empty {
            field: "image.data"
        })
    );
}

#[test]
fn a_file_part_needs_a_media_type_and_a_non_empty_name() {
    let part = ContentPart::File {
        media_type: "text".to_owned(),
        data: vec![1],
        name: None,
    };
    assert!(matches!(
        part.validate(),
        Err(SchemaError::Invalid {
            field: "content_part.media_type",
            ..
        })
    ));
    let part = ContentPart::File {
        media_type: "text/plain".to_owned(),
        data: vec![1],
        name: Some("  ".to_owned()),
    };
    assert_eq!(
        part.validate(),
        Err(SchemaError::Empty {
            field: "content_part.name"
        })
    );
}

#[test]
fn a_canonical_error_has_a_stable_code_and_status() {
    assert_eq!(
        CanonicalError::try_new(ErrorCategory::Upstream, "", "failed", 502)
            .expect_err("empty code"),
        SchemaError::Empty {
            field: "canonical_error.code"
        }
    );
    assert!(matches!(
        CanonicalError::try_new(ErrorCategory::Upstream, "Bad-Code", "failed", 502),
        Err(SchemaError::Invalid {
            field: "canonical_error.code",
            ..
        })
    ));
    assert_eq!(
        CanonicalError::try_new(ErrorCategory::Upstream, "upstream_failed", "failed", 200)
            .expect_err("2xx is not an error"),
        SchemaError::OutOfRange {
            field: "canonical_error.http_status",
            value: "200".to_owned()
        }
    );
    let error = CanonicalError::try_new(
        ErrorCategory::Upstream,
        "upstream_failed",
        "the provider failed",
        502,
    )
    .expect("valid error");
    assert!(!error.retryable());
    assert_eq!(
        error
            .with_provider_code("")
            .expect_err("empty provider code"),
        SchemaError::Empty {
            field: "canonical_error.provider_code"
        }
    );
}

#[test]
fn a_canonical_error_message_is_bounded() {
    let message = "x".repeat(257);
    assert!(matches!(
        CanonicalError::try_new(ErrorCategory::Internal, "internal", message, 500),
        Err(SchemaError::TooLong {
            field: "canonical_error.message",
            limit: 256,
            ..
        })
    ));
}

#[test]
fn raw_json_text_must_parse() {
    assert!(matches!(
        RawJson::parse("{"),
        Err(SchemaError::InvalidJson {
            field: "raw_json",
            ..
        })
    ));
}

#[test]
fn a_rejected_value_is_echoed_as_a_bounded_excerpt() {
    let payload = format!("data:image/png;base64,{}", "A".repeat(4096));
    let error = ImageSource::Url(payload)
        .validate()
        .expect_err("a data URL is not a remote image");
    let rendered = error.to_string();
    assert!(
        rendered.len() < 160,
        "a diagnostic must not carry the payload: {rendered}"
    );
    assert!(
        !rendered.contains(&"A".repeat(128)),
        "the payload tail must be cut off: {rendered}"
    );

    // The bound falls inside a multi-byte character: the excerpt still has to be valid UTF-8.
    let multibyte = format!("ftp://x{}", "é".repeat(64));
    let rendered = ImageSource::Url(multibyte)
        .validate()
        .expect_err("a non-http scheme is rejected")
        .to_string();
    assert!(rendered.contains("ftp://x"), "the scheme stays diagnosable");
}

#[test]
fn an_unknown_usage_token_kind_is_rejected() {
    let error =
        serde_json::from_str::<UsageCounts>(r#"{"prompt":1}"#).expect_err("unknown token kind");
    assert!(
        error.to_string().contains("prompt"),
        "the rejection must name the unknown kind: {error}"
    );
}
