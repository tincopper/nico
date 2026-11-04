use super::*;

fn route_with_host(host: &str) -> Result<Document, DocumentError> {
    with_routes(&format!(
        concat!(
            "  - id: api-public-v1\n",
            "    host: {}\n",
            "    path_prefix: /api/\n",
            "    upstream: api-backend\n",
        ),
        host
    ))
}

#[test]
fn a_host_with_a_port_is_rejected() {
    let error = route_with_host("api.example:8080").expect_err("host carries a port");
    assert!(
        matches!(
            error,
            DocumentError::InvalidField {
                field: "routes[].host",
                ..
            }
        ),
        "expected InvalidField for routes[].host, got {error:?}"
    );
}

#[test]
fn a_host_with_a_scheme_is_rejected() {
    let error = route_with_host("https://api.example").expect_err("host carries a scheme");
    assert!(
        matches!(
            error,
            DocumentError::InvalidField {
                field: "routes[].host",
                ..
            }
        ),
        "expected InvalidField for routes[].host, got {error:?}"
    );
}

#[test]
fn a_host_with_whitespace_is_rejected() {
    let error = route_with_host("\"api example\"").expect_err("host carries a space");
    assert!(
        matches!(
            error,
            DocumentError::InvalidField {
                field: "routes[].host",
                ..
            }
        ),
        "expected InvalidField for routes[].host, got {error:?}"
    );
}

#[test]
fn an_empty_host_is_rejected() {
    let error = route_with_host("\"\"").expect_err("host is empty");
    assert!(
        matches!(
            error,
            DocumentError::InvalidField {
                field: "routes[].host",
                ..
            }
        ),
        "expected InvalidField for routes[].host, got {error:?}"
    );
}

#[test]
fn a_host_boundary_is_253_bytes() {
    let at_bound = "a".repeat(253);
    assert!(
        route_with_host(&at_bound).is_ok(),
        "a 253-byte host must be accepted"
    );

    let over_bound = "a".repeat(254);
    let error = route_with_host(&over_bound).expect_err("254-byte host");
    assert!(
        matches!(
            error,
            DocumentError::InvalidField {
                field: "routes[].host",
                ..
            }
        ),
        "expected InvalidField for routes[].host, got {error:?}"
    );
}
