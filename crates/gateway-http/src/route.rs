//! Route selection: which route a request matches, decided from request headers only.
//!
//! Selection is one linear scan over the document's routes. That is deliberate: routes are already
//! typed, validated values, so matching is integer comparison and byte-prefix checks on borrowed
//! data — no parsing, no allocation, no second copy of the table to invalidate when a new generation
//! is published. A compiled index would add a compiled artefact and a rebuild on every reload
//! without removing any work this scan does.
//!
//! # Precedence
//!
//! 1. A route that names a `host` only matches that host, compared case-insensitively and ignoring
//!    the port the client sent; a route without one matches any host.
//! 2. A host-specific match beats a host-agnostic match, whatever their prefixes are.
//! 3. Among routes of the same host rank, the longer `path_prefix` wins.
//! 4. When the rank and the prefix length are both equal, the earlier document entry wins.
//!
//! `path_prefix` is compared as bytes: `/api/` does not match `/api`, and `/api` matches `/apix`.
//! Normalising paths here would be a routing policy, not a lookup.

use gateway_core::Route;
use pingora::http::{RequestHeader, authority::raw_target_authority};

/// Returns the route a request matches, or `None` when no route accepts it.
///
/// The request body is never consulted, so the result is available before the first body byte
/// arrives. A target with no path — the authority-form `CONNECT` target and the asterisk-form
/// `OPTIONS *` — matches no prefix, so it is not routed.
///
/// # Examples
///
/// ```
/// use gateway_core::{Document, ParseLimits};
/// use gateway_http::match_route;
/// use pingora::http::RequestHeader;
///
/// let yaml = "\
/// apiVersion: gateway/v1
/// limits:
///   max_request_body_bytes: 1048576
///   request_timeout_ms: 30000
/// providers: []
/// upstreams:
///   - name: api-backend
///     scheme: http
///     host: 127.0.0.1
///     port: 8080
/// routes:
///   - id: api
///     path_prefix: /api
///     upstream: api-backend
///   - id: api-v1
///     path_prefix: /api/v1/
///     upstream: api-backend
/// ";
/// let document = Document::parse(yaml.as_bytes(), ParseLimits::new(4096)).expect("fixture parses");
/// let request = RequestHeader::build("GET", b"/api/v1/items", None).expect("request builds");
///
/// let matched = match_route(&request, document.routes()).expect("a route matches");
/// assert_eq!(matched.id(), "api-v1", "the longer prefix wins");
/// ```
#[must_use]
pub fn match_route<'a>(header: &RequestHeader, routes: &'a [Route]) -> Option<&'a Route> {
    let request_host = request_host(header);
    let path = header.uri.path();

    let mut best: Option<&Route> = None;
    let mut best_is_host_specific = false;

    for route in routes {
        let is_host_specific = match route.host() {
            // A route tied to a host is not a candidate for any other host.
            Some(expected) => match request_host {
                Some(actual) if actual.eq_ignore_ascii_case(expected) => true,
                _ => continue,
            },
            None => false,
        };

        if !path.starts_with(route.path_prefix()) {
            continue;
        }

        // Ranking is `(host rank, prefix length)`, compared lexicographically: a host-specific route
        // always outranks a host-agnostic one, and the prefix only breaks ties within a rank. `>` (not
        // `>=`) keeps the earlier document entry when both components are equal.
        let incumbent = best.map(|route| (best_is_host_specific, route.path_prefix().len()));
        let candidate = (is_host_specific, route.path_prefix().len());
        if incumbent.is_none_or(|incumbent| candidate > incumbent) {
            best = Some(route);
            best_is_host_specific = is_host_specific;
        }
    }

    best
}

/// Returns the host the client addressed, with any port removed.
///
/// The precedence mirrors the one Pingora's own proxy uses when it reconciles the target against
/// the `Host` header, so routing and forwarding cannot disagree about who the client addressed:
///
/// 1. the URI authority — an HTTP/2 request whose target carried one, or a header built from a
///    `Request`;
/// 2. the authority of an HTTP/1 absolute-form request target, which Pingora keeps verbatim and
///    deliberately leaves off the URI;
/// 3. the `Host` header — origin-form HTTP/1, and HTTP/2, whose `:authority` Pingora lands in
///    `Host`.
///
/// A non-UTF-8 authority matches no route host: route hosts are text, so there is nothing to
/// compare it with.
fn request_host(header: &RequestHeader) -> Option<&str> {
    if let Some(host) = header.uri.host() {
        return Some(without_port(host));
    }
    if let Some(authority) = raw_target_authority(header.raw_path()).authority() {
        return std::str::from_utf8(authority).ok().map(without_port);
    }
    header
        .headers
        .get("host")
        .and_then(|value| value.to_str().ok())
        .map(without_port)
}

/// Removes a trailing `:port`, keeping an IPv6 literal's brackets and its inner colons.
fn without_port(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        return match rest.find(']') {
            Some(end) => &host[..end + 2],
            None => host,
        };
    }
    match host.find(':') {
        Some(end) => &host[..end],
        None => host,
    }
}

#[cfg(test)]
mod tests;
