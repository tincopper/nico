use super::*;
use crate::config::{ExpectedStatus, ProbeMethod};

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

/// An upstream entry with a `health_check` section appended.
fn with_health_check(section: &str) -> Result<Document, DocumentError> {
    let upstream = format!(
        "  - name: api-backend\n    scheme: http\n    host: 127.0.0.1\n    port: 8080\n    health_check:\n      path: /health\n      interval_ms: 5000\n      timeout_ms: 1000\n{}",
        section
    );
    parse(&document(NO_ENTRIES, &upstream, NO_ENTRIES))
}

#[test]
fn a_health_check_with_only_required_fields_is_accepted() {
    let document = with_health_check("").expect("the minimal check parses");
    let check = document
        .upstreams()
        .first()
        .expect("one upstream")
        .health_check()
        .expect("the document enables the check");
    assert_eq!(check.method(), ProbeMethod::GET);
    assert_eq!(check.path(), "/health");
    assert_eq!(check.expect(), ExpectedStatus::Non5xx);
    assert_eq!(check.interval_ms(), 5000);
    assert_eq!(
        check.no_traffic_interval_ms(),
        5000,
        "an idle cadence that wrote nothing is the written interval"
    );
    assert_eq!(check.timeout_ms(), 1000);
    assert_eq!(check.unhealthy_threshold(), 1);
    assert_eq!(check.healthy_threshold(), 1);
}

#[test]
fn a_health_check_with_every_field_is_read_as_written() {
    let document = with_health_check(
        "      method: HEAD\n      expect: 2xx\n      no_traffic_interval_ms: 30000\n      unhealthy_threshold: 3\n      healthy_threshold: 2\n",
    )
    .expect("the full check parses");
    let check = document
        .upstreams()
        .first()
        .expect("one upstream")
        .health_check()
        .expect("the check is present");
    assert_eq!(check.method(), ProbeMethod::HEAD);
    assert_eq!(check.expect(), ExpectedStatus::S2xx);
    assert_eq!(check.no_traffic_interval_ms(), 30000);
    assert_eq!(check.unhealthy_threshold(), 3);
    assert_eq!(check.healthy_threshold(), 2);
}

#[test]
fn an_upstream_without_a_health_check_has_none() {
    let document = with_upstream("127.0.0.1", 8080).expect("the plain upstream parses");
    assert!(
        document
            .upstreams()
            .first()
            .expect("one upstream")
            .health_check()
            .is_none(),
        "no section means no probing"
    );
}

#[test]
fn a_health_check_path_that_is_not_a_path_is_rejected() {
    for bad in ["health", "\"\"", "\"/health?x=1\"", "\"/health#frag\""] {
        let document = with_health_check(&format!(
            "      path: {bad}\n      interval_ms: 5000\n      timeout_ms: 1000\n"
        ));
        // path is required: replacing the base path means building the entry differently —
        // simpler to test via string surgery below.
        let _ = document;
    }
    // Path variants via direct entry construction:
    for (bad, expected_field) in [
        ("health", "upstreams[].health_check.path"),
        ("\"\"", "upstreams[].health_check.path"),
    ] {
        let upstream = format!(
            "  - name: api-backend\n    scheme: http\n    host: 127.0.0.1\n    port: 8080\n    health_check:\n      path: {bad}\n      interval_ms: 5000\n      timeout_ms: 1000\n"
        );
        let error =
            parse(&document(NO_ENTRIES, &upstream, NO_ENTRIES)).expect_err("the path is refused");
        assert!(
            matches!(
                error,
                DocumentError::InvalidField { field, .. } if field == expected_field
            ),
            "expected InvalidField for {expected_field}, got {error:?}"
        );
    }
}

#[test]
fn a_health_check_path_with_a_query_or_fragment_is_rejected() {
    for bad in ["\"/health?verbose\"", "\"/health#now\""] {
        let upstream = format!(
            "  - name: api-backend\n    scheme: http\n    host: 127.0.0.1\n    port: 8080\n    health_check:\n      path: {bad}\n      interval_ms: 5000\n      timeout_ms: 1000\n"
        );
        let error =
            parse(&document(NO_ENTRIES, &upstream, NO_ENTRIES)).expect_err("the path is refused");
        assert!(
            matches!(
                error,
                DocumentError::InvalidField {
                    field: "upstreams[].health_check.path",
                    ..
                }
            ),
            "expected InvalidField, got {error:?}"
        );
    }
}

#[test]
fn a_zero_health_period_or_threshold_is_rejected() {
    for (field, line) in [
        (
            "upstreams[].health_check.interval_ms",
            "      interval_ms: 0\n",
        ),
        (
            "upstreams[].health_check.timeout_ms",
            "      timeout_ms: 0\n",
        ),
        (
            "upstreams[].health_check.no_traffic_interval_ms",
            "      no_traffic_interval_ms: 0\n",
        ),
        (
            "upstreams[].health_check.unhealthy_threshold",
            "      unhealthy_threshold: 0\n",
        ),
        (
            "upstreams[].health_check.healthy_threshold",
            "      healthy_threshold: 0\n",
        ),
    ] {
        // The zeroed field replaces its base line, so the fixture is built from the keys the
        // case does not zero (a duplicate key is a different error).
        let mut base = String::from(
            "  - name: api-backend\n    scheme: http\n    host: 127.0.0.1\n    port: 8080\n    health_check:\n      path: /health\n",
        );
        if !line.starts_with("      interval_ms:") {
            base.push_str("      interval_ms: 5000\n");
        }
        if !line.starts_with("      timeout_ms:") {
            base.push_str("      timeout_ms: 1000\n");
        }
        let upstream = format!("{base}{line}");
        let error = parse(&document(NO_ENTRIES, &upstream, NO_ENTRIES))
            .expect_err("a zero period is refused");
        assert!(
            matches!(
                error,
                DocumentError::LimitOutOfRange { field: named, .. } if named == field
            ),
            "expected LimitOutOfRange for {field}, got {error:?}"
        );
    }
}

#[test]
fn a_health_period_above_the_ceiling_is_rejected() {
    let upstream = "  - name: api-backend\n    scheme: http\n    host: 127.0.0.1\n    port: 8080\n    health_check:\n      path: /health\n      interval_ms: 600001\n      timeout_ms: 1000\n";
    let error = parse(&document(NO_ENTRIES, upstream, NO_ENTRIES))
        .expect_err("a period over ten minutes is refused");
    assert!(
        matches!(
            error,
            DocumentError::LimitOutOfRange {
                field: "upstreams[].health_check.interval_ms",
                ..
            }
        ),
        "expected LimitOutOfRange, got {error:?}"
    );
}

#[test]
fn an_unknown_health_check_field_is_rejected() {
    let upstream = "  - name: api-backend\n    scheme: http\n    host: 127.0.0.1\n    port: 8080\n    health_check:\n      path: /health\n      interval_ms: 5000\n      timeout_ms: 1000\n      match: welcome\n";
    let error = parse(&document(NO_ENTRIES, upstream, NO_ENTRIES))
        .expect_err("a match body is not this document's vocabulary");
    assert!(matches!(error, DocumentError::Yaml { .. }), "got {error:?}");
}
