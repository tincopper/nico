use super::*;

#[test]
fn a_name_with_uppercase_letters_is_rejected() {
    let error = with_routes(&route("Api-Public", "/api/")).expect_err("uppercase route id");
    assert!(
        matches!(
            error,
            DocumentError::InvalidField {
                field: "routes[].id",
                ..
            }
        ),
        "expected InvalidField for routes[].id, got {error:?}"
    );
    assert_eq!(error.category(), ErrorCategory::Client);
}

#[test]
fn an_underscore_in_an_upstream_name_is_rejected() {
    let upstreams = concat!(
        "  - name: api_backend\n",
        "    scheme: http\n",
        "    host: 127.0.0.1\n",
        "    port: 8080\n",
    );
    let error = parse(&document(PROVIDER, upstreams, NO_ENTRIES))
        .expect_err("underscore in an upstream name");
    assert!(
        matches!(
            error,
            DocumentError::InvalidField {
                field: "upstreams[].name",
                ..
            }
        ),
        "expected InvalidField for upstreams[].name, got {error:?}"
    );
}

#[test]
fn an_underscore_in_a_provider_name_is_rejected() {
    let providers = concat!(
        "  - name: openai_primary\n",
        "    kind: openai\n",
        "    base_url: https://api.openai.com\n",
        "    credential_ref: env:OPENAI_API_KEY\n",
    );
    let error = parse(&document(providers, NO_ENTRIES, NO_ENTRIES)).expect_err("bad provider name");
    assert!(
        matches!(
            error,
            DocumentError::InvalidField {
                field: "providers[].name",
                ..
            }
        ),
        "expected InvalidField for providers[].name, got {error:?}"
    );
}

#[test]
fn an_empty_name_is_rejected() {
    let error = with_routes(&route("\"\"", "/api/")).expect_err("empty route id");
    assert!(
        matches!(
            error,
            DocumentError::InvalidField {
                field: "routes[].id",
                ..
            }
        ),
        "expected InvalidField for routes[].id, got {error:?}"
    );
}

#[test]
fn a_name_boundary_is_63_bytes() {
    let at_bound = "a".repeat(63);
    assert!(
        with_routes(&route(&at_bound, "/api/")).is_ok(),
        "a 63-byte name must be accepted"
    );

    let over_bound = "a".repeat(64);
    let error = with_routes(&route(&over_bound, "/api/")).expect_err("64-byte route id");
    assert!(
        matches!(
            error,
            DocumentError::InvalidField {
                field: "routes[].id",
                ..
            }
        ),
        "expected InvalidField for routes[].id, got {error:?}"
    );
}
