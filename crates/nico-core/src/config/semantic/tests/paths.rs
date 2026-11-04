use super::*;

#[test]
fn a_path_prefix_without_a_leading_slash_is_rejected() {
    let error = with_routes(&route("api-public-v1", "api/")).expect_err("prefix is not rooted");
    assert!(
        matches!(
            error,
            DocumentError::InvalidField {
                field: "routes[].path_prefix",
                ..
            }
        ),
        "expected InvalidField for routes[].path_prefix, got {error:?}"
    );
}

#[test]
fn a_path_prefix_with_a_parent_segment_is_rejected() {
    let error =
        with_routes(&route("api-public-v1", "/api/../etc")).expect_err("prefix escapes its root");
    assert!(
        matches!(
            error,
            DocumentError::InvalidField {
                field: "routes[].path_prefix",
                ..
            }
        ),
        "expected InvalidField for routes[].path_prefix, got {error:?}"
    );
}

#[test]
fn a_path_prefix_with_whitespace_is_rejected() {
    let error =
        with_routes(&route("api-public-v1", "\"/api/ v1\"")).expect_err("prefix carries a space");
    assert!(
        matches!(
            error,
            DocumentError::InvalidField {
                field: "routes[].path_prefix",
                ..
            }
        ),
        "expected InvalidField for routes[].path_prefix, got {error:?}"
    );
}

#[test]
fn a_path_prefix_boundary_is_256_bytes() {
    let at_bound = format!("/{}", "a".repeat(255));
    assert_eq!(at_bound.len(), 256);
    assert!(
        with_routes(&route("api-public-v1", &at_bound)).is_ok(),
        "a 256-byte prefix must be accepted"
    );

    let over_bound = format!("/{}", "a".repeat(256));
    let error = with_routes(&route("api-public-v1", &over_bound)).expect_err("257-byte prefix");
    assert!(
        matches!(
            error,
            DocumentError::InvalidField {
                field: "routes[].path_prefix",
                ..
            }
        ),
        "expected InvalidField for routes[].path_prefix, got {error:?}"
    );
}
