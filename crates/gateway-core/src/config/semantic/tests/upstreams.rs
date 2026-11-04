use super::*;

/// Builds a single upstream entry whose host and port are variable.
fn upstream_entry(host: &str, port: u16) -> String {
    format!(
        concat!(
            "  - name: api-backend\n",
            "    scheme: http\n",
            "    host: {}\n",
            "    port: {}\n",
        ),
        host, port
    )
}

/// Parses a document whose only upstream is `upstream_entry(host, port)`.
fn with_upstream(host: &str, port: u16) -> Result<Document, DocumentError> {
    parse(&document(
        NO_ENTRIES,
        &upstream_entry(host, port),
        NO_ENTRIES,
    ))
}

#[test]
fn a_zero_port_is_rejected() {
    let error = with_upstream("127.0.0.1", 0).expect_err("port 0 is not routable");
    assert!(
        matches!(
            error,
            DocumentError::InvalidField {
                field: "upstreams[].port",
                ..
            }
        ),
        "expected InvalidField for upstreams[].port, got {error:?}"
    );
}

#[test]
fn an_empty_upstream_host_is_rejected() {
    let error = with_upstream("\"\"", 8080).expect_err("an empty host has no destination");
    assert!(
        matches!(
            error,
            DocumentError::InvalidField {
                field: "upstreams[].host",
                ..
            }
        ),
        "expected InvalidField for upstreams[].host, got {error:?}"
    );
}

#[test]
fn an_upstream_host_with_a_scheme_is_rejected() {
    let error = with_upstream("http://api.example", 8080).expect_err("the scheme is its own field");
    assert!(
        matches!(
            error,
            DocumentError::InvalidField {
                field: "upstreams[].host",
                ..
            }
        ),
        "expected InvalidField for upstreams[].host, got {error:?}"
    );
}

#[test]
fn an_upstream_host_with_a_port_is_rejected() {
    let error = with_upstream("api.example:8080", 8080).expect_err("the port is its own field");
    assert!(
        matches!(
            error,
            DocumentError::InvalidField {
                field: "upstreams[].host",
                ..
            }
        ),
        "expected InvalidField for upstreams[].host, got {error:?}"
    );
}

#[test]
fn an_upstream_host_with_whitespace_is_rejected() {
    let error = with_upstream("\"api example\"", 8080).expect_err("a host is one token");
    assert!(
        matches!(
            error,
            DocumentError::InvalidField {
                field: "upstreams[].host",
                ..
            }
        ),
        "expected InvalidField for upstreams[].host, got {error:?}"
    );
}

#[test]
fn an_upstream_host_boundary_is_253_bytes() {
    let at_bound = "a".repeat(253);
    assert!(
        with_upstream(&at_bound, 8080).is_ok(),
        "a 253-byte host must be accepted"
    );

    let over_bound = "a".repeat(254);
    let error = with_upstream(&over_bound, 8080).expect_err("254-byte host");
    assert!(
        matches!(
            error,
            DocumentError::InvalidField {
                field: "upstreams[].host",
                ..
            }
        ),
        "expected InvalidField for upstreams[].host, got {error:?}"
    );
}
