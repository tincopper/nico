//! Semantic checks for the G4 proxy configuration fields.

use super::{
    super::{
        error::DocumentError,
        route::{ProxyHeaderValue, Route},
    },
    invalid,
};

/// Checks the value that replaces a route's matched `path_prefix` at the forward.
pub(super) fn validate_rewrite_prefix(value: Option<&str>) -> Result<(), DocumentError> {
    const FIELD: &str = "routes[].rewrite_prefix";
    let Some(value) = value else {
        return Ok(());
    };
    if !value.starts_with('/') {
        return Err(invalid(
            FIELD,
            "must be a path starting with `/`; stripping the whole prefix is `/`",
        ));
    }
    if value.contains(['?', '#']) || has_whitespace(value) {
        return Err(invalid(
            FIELD,
            "must be a bare path: no query, fragment, whitespace or control characters",
        ));
    }
    Ok(())
}

/// Checks the request-header rewrites a route declares.
pub(super) fn validate_proxy_headers(route: &Route) -> Result<(), DocumentError> {
    const SET_FIELD: &str = "routes[].proxy_set_header[].name";
    for entry in route.proxy_set_header() {
        let lowered = entry.name().to_ascii_lowercase();
        if !lowered
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(invalid(
                SET_FIELD,
                format!("`{}` is not a header name token", entry.name()),
            ));
        }
        if matches!(lowered.as_str(), "content-length" | "transfer-encoding") {
            return Err(invalid(
                SET_FIELD,
                format!("`{lowered}` is framing the data plane owns"),
            ));
        }
        if let ProxyHeaderValue::Literal(value) = entry.value()
            && !value
                .bytes()
                .all(|byte| (32..127).contains(&byte) || byte == b'\t')
        {
            return Err(invalid(
                "routes[].proxy_set_header[].value",
                "must be visible ASCII, space or tab",
            ));
        }
    }
    for name in route.proxy_del_header() {
        let lowered = name.to_ascii_lowercase();
        if !lowered
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(invalid(
                "routes[].proxy_del_header[]",
                format!("`{name}` is not a header name token"),
            ));
        }
    }
    Ok(())
}

fn has_whitespace(value: &str) -> bool {
    value.chars().any(char::is_whitespace)
}
