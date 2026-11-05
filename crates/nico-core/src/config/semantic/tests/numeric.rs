use super::*;

#[test]
fn a_zero_route_timeout_is_rejected() {
    let routes = format!("{}    timeout_ms: 0\n", route("api-public-v1", "/api/"));
    let error = with_routes(&routes).expect_err("a zero timeout is not a timeout");
    assert!(
        matches!(
            error,
            DocumentError::LimitOutOfRange {
                field: "routes[].timeout_ms",
                value: 0
            }
        ),
        "expected LimitOutOfRange for routes[].timeout_ms, got {error:?}"
    );
    assert_eq!(error.category(), ErrorCategory::Limit);
}

#[test]
fn a_route_timeout_above_ten_minutes_is_rejected() {
    let routes = format!(
        "{}    timeout_ms: 600001\n",
        route("api-public-v1", "/api/")
    );
    let error = with_routes(&routes).expect_err("timeout above the bound");
    assert!(
        matches!(
            error,
            DocumentError::LimitOutOfRange {
                field: "routes[].timeout_ms",
                value: 600_001
            }
        ),
        "expected LimitOutOfRange for routes[].timeout_ms, got {error:?}"
    );
}

#[test]
fn a_zero_provider_timeout_is_rejected() {
    let providers = format!(
        "{}    timeout_ms: 0\n",
        provider_entry("https://api.openai.com", "env:OPENAI_API_KEY")
    );
    let error = parse(&document(&providers, NO_ENTRIES, NO_ENTRIES))
        .expect_err("a zero timeout is not a timeout");
    assert!(
        matches!(
            error,
            DocumentError::LimitOutOfRange {
                field: "providers[].timeout_ms",
                value: 0
            }
        ),
        "expected LimitOutOfRange for providers[].timeout_ms, got {error:?}"
    );
}

#[test]
fn a_route_body_bound_above_the_document_bound_is_rejected() {
    let routes = format!(
        "{}    max_body_bytes: 2000000\n",
        route("api-public-v1", "/api/")
    );
    let error = with_routes(&routes).expect_err("above request.max_body_bytes");
    assert!(
        matches!(
            error,
            DocumentError::LimitOutOfRange {
                field: "routes[].max_body_bytes",
                value: 2_000_000
            }
        ),
        "expected LimitOutOfRange for routes[].max_body_bytes, got {error:?}"
    );
}

#[test]
fn a_zero_route_body_bound_is_accepted() {
    let routes = format!("{}    max_body_bytes: 0\n", route("api-public-v1", "/api/"));
    let document = with_routes(&routes).expect("0 is the tightest legal bound: no body at all");
    assert_eq!(document.routes()[0].max_body_bytes(), Some(0));
}

#[test]
fn a_route_body_bound_at_the_document_bound_is_accepted() {
    let routes = format!(
        "{}    max_body_bytes: 1048576\n",
        route("api-public-v1", "/api/")
    );
    let document = with_routes(&routes).expect("a bound equal to the document bound is legal");
    assert_eq!(document.routes()[0].max_body_bytes(), Some(1_048_576));
}
