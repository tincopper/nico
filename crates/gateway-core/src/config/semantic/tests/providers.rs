use super::*;

#[test]
fn a_base_url_without_an_http_scheme_is_rejected() {
    let providers = provider_entry("ftp://api.openai.com", "env:OPENAI_API_KEY");
    let error =
        parse(&document(&providers, NO_ENTRIES, NO_ENTRIES)).expect_err("non-HTTP base URL");
    assert!(
        matches!(
            error,
            DocumentError::InvalidField {
                field: "providers[].base_url",
                ..
            }
        ),
        "expected InvalidField for providers[].base_url, got {error:?}"
    );
}

#[test]
fn a_base_url_with_whitespace_is_rejected() {
    let providers = provider_entry("https://api openai.com", "env:OPENAI_API_KEY");
    let error =
        parse(&document(&providers, NO_ENTRIES, NO_ENTRIES)).expect_err("whitespace in URL");
    assert!(
        matches!(
            error,
            DocumentError::InvalidField {
                field: "providers[].base_url",
                ..
            }
        ),
        "expected InvalidField for providers[].base_url, got {error:?}"
    );
}

#[test]
fn a_credential_ref_without_a_known_prefix_is_rejected() {
    let providers = provider_entry("https://api.openai.com", "OPENAI_API_KEY");
    let error = parse(&document(&providers, NO_ENTRIES, NO_ENTRIES))
        .expect_err("a literal credential is not a reference");
    assert!(
        matches!(
            error,
            DocumentError::InvalidField {
                field: "providers[].credential_ref",
                ..
            }
        ),
        "expected InvalidField for providers[].credential_ref, got {error:?}"
    );
}
