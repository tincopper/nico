use super::*;

#[test]
fn a_repeated_host_and_path_prefix_is_rejected() {
    let routes = format!(
        "{}{}",
        route("api-public-v1", "/api/"),
        route("api-public-v2", "/api/")
    );
    let error = with_routes(&routes).expect_err("both routes match the same requests");
    assert!(
        matches!(
            error,
            DocumentError::DuplicateId {
                kind: "route-match",
                ref id
            } if id == "*/api/"
        ),
        "expected DuplicateId for the match combination, got {error:?}"
    );
    assert_eq!(error.category(), ErrorCategory::Client);
}

#[test]
fn a_repeated_hosted_match_is_reported_with_the_host() {
    let hosted = |id: &str| {
        format!(
            concat!(
                "  - id: {}\n",
                "    host: api.example\n",
                "    path_prefix: /api/\n",
                "    upstream: api-backend\n",
            ),
            id
        )
    };
    let routes = format!("{}{}", hosted("api-public-v1"), hosted("api-public-v2"));
    let error = with_routes(&routes).expect_err("same host and prefix");
    assert!(
        matches!(
            error,
            DocumentError::DuplicateId {
                kind: "route-match",
                ref id
            } if id == "api.example/api/"
        ),
        "expected DuplicateId naming the host, got {error:?}"
    );
}
