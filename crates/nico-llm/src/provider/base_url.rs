//! Reading a route's `base_url` as a destination and the API prefix an operation path is appended to.
//!
//! A base URL is an *API prefix*, not a request target: a route names a provider and its prefix, and
//! the operation is fixed by the adapter, because [`Endpoint`](super::Endpoint) is resolved before
//! the request body is decoded. The rules are therefore closed, and they are the same for every
//! adapter — only the paths differ, and those stay in each vendor module:
//!
//! - `http://host` or `https://host` (scheme in any case, default ports 80 and 443) names an API
//!   root; the adapter supplies the operation's default path.
//! - Any base path is a prefix: `.../v1` yields `<prefix>` + the adapter's suffix, and
//!   `.../api` yields `/api` + the suffix. Trailing slashes are ignored, and a prefix that still
//!   carries an empty segment (`.../api//v1`) is refused rather than sent as a request
//!   target whose meaning the origin would have to decide — a leading `//` reads as a network-path
//!   reference to some peers.
//! - Every part of a URL this crate does not interpret is refused rather than dropped: user
//!   information (the credential belongs in `credential_ref`, and a URL that carries a second secret
//!   would put it where nothing logs or rotates it), a query, a fragment, an empty host, a port that
//!   is not a number or is zero, and whitespace or a control character anywhere.
//! - The host itself is [`nico_core::validate_host`]'s rule, the same one a document's
//!   `upstreams[].host` is held to. That rule refuses a colon-bearing host — a port, or an IPv6
//!   literal — because a destination carries a bare name and the data plane writes `host:port`
//!   without re-bracketing, so `[::1]` would become a `Host` value nothing can read. An IPv6 literal
//!   is therefore refused here today; the bracket returns on the day a destination can address one.
//!
//! Parsing is hand-written on purpose: the value is a narrow `scheme://host[:port][/path]` form, and
//! a general URL parser would add a dependency and accept forms (user information, encoded hosts,
//! relative references) that an adapter would then have to re-refuse.

use std::borrow::Cow;

use nico_core::{Scheme, validate_host};
use nico_http::Destination;

use super::EndpointError;

/// The dotted configuration field every base-URL failure names.
pub(crate) const FIELD: &str = "provider.base_url";

/// One parsed base URL: where to connect, and the prefix an operation path keeps.
pub(crate) struct Base {
    /// The peer to connect to; TLS policy and budgets stay with the data plane.
    pub(crate) destination: Destination,
    /// The trimmed base path, empty when the URL names an API root.
    prefix: String,
}

impl Base {
    /// Returns the request path this base URL and one operation resolve to.
    ///
    /// `default_path` is what an API root means for the operation, and `suffix` is what a prefix
    /// gains: the join never produces `//`, because [`Base::prefix`] is trimmed and checked when the
    /// URL is parsed.
    pub(crate) fn path(&self, default_path: &str, suffix: &str) -> String {
        if self.prefix.is_empty() {
            default_path.to_owned()
        } else {
            format!("{}{suffix}", self.prefix)
        }
    }
}

/// Parses the `scheme://authority/path` form this crate accepts.
///
/// # Errors
/// Returns [`EndpointError::BaseUrl`] for each rule the value breaks; the reason names the rule and
/// never quotes the value, which can carry user information.
pub(crate) fn parse(base_url: &str) -> Result<Base, EndpointError> {
    if base_url
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(fail("the URL holds whitespace or a control character"));
    }
    let (scheme, default_port, rest) = match strip_prefix_ascii(base_url, "http://") {
        Some(rest) => (Scheme::Http, 80, rest),
        None => match strip_prefix_ascii(base_url, "https://") {
            Some(rest) => (Scheme::Https, 443, rest),
            None => {
                return Err(fail("the URL must begin with http:// or https://"));
            }
        },
    };
    if rest.contains('?') || rest.contains('#') {
        return Err(fail("the URL carries a query or a fragment"));
    }
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let path = &rest[authority_end..];
    if authority.is_empty() {
        return Err(fail("the URL names no host"));
    }
    if authority.contains('@') {
        return Err(fail("the URL carries user information"));
    }
    let (host, port) = split_authority(authority)?;
    validate_host(&host).map_err(|problem| fail(problem.reason()))?;
    Ok(Base {
        destination: Destination::new(scheme, host, port.unwrap_or(default_port)),
        prefix: prefix(path)?,
    })
}

/// Builds the one base-URL failure a parse reports.
pub(crate) fn fail(reason: impl Into<Cow<'static, str>>) -> EndpointError {
    EndpointError::BaseUrl {
        field: FIELD,
        reason: reason.into(),
    }
}

/// Trims the base path of its trailing slashes and refuses one that keeps an empty segment.
///
/// The join itself never produces `//`. A prefix that carries an empty segment of its own is refused
/// instead: the adapter will not hand an origin a path it would have to reinterpret, and a leading
/// `//` is read as a network-path reference by some peers.
fn prefix(base_path: &str) -> Result<String, EndpointError> {
    let prefix = base_path.trim_end_matches('/');
    if prefix.contains("//") {
        return Err(fail("the path carries an empty segment"));
    }
    Ok(prefix.to_owned())
}

/// Splits `host[:port]`, refusing the bracketed IPv6 form.
///
/// The host rule has no place for a colon-bearing value, and a destination is written as
/// `host:port`, so an accepted literal would be an unaddressable `Host`. The bracket returns here on
/// the day a destination can address one; until then it is refused where it is recognised.
fn split_authority(authority: &str) -> Result<(String, Option<u16>), EndpointError> {
    if authority.contains('[') || authority.contains(']') {
        return Err(fail("the host must not be an IPv6 literal"));
    }
    let (host, port) = match authority.split_once(':') {
        Some((host, digits)) => (host, Some(parse_port(digits)?)),
        None => (authority, None),
    };
    Ok((host.to_owned(), port))
}

/// Parses an explicit port, refusing the forms a destination cannot carry.
fn parse_port(digits: &str) -> Result<u16, EndpointError> {
    if digits.is_empty() {
        return Err(fail("the URL names an empty port"));
    }
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(fail("the URL names a port that is not a number"));
    }
    let port = digits
        .parse::<u16>()
        .map_err(|_| fail(format!("the URL names a port above {}", u16::MAX)))?;
    if port == 0 {
        return Err(fail("the URL names port zero"));
    }
    Ok(port)
}

/// Strips an ASCII prefix, ignoring case.
fn strip_prefix_ascii<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    let head = value.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        Some(&value[prefix.len()..])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use nico_core::Scheme;

    use super::parse;
    use crate::provider::EndpointError;

    /// The refusal of a URL the parser must not accept.
    fn invalid(base_url: &str) -> EndpointError {
        parse(base_url).err().expect("the URL must be refused")
    }

    #[test]
    fn an_origin_names_an_api_root_and_keeps_the_destinations_parts() {
        let base = parse("HTTP://api.example.test:8080/").expect("a valid URL");
        assert_eq!(base.destination.scheme(), Scheme::Http);
        assert_eq!(base.destination.host(), "api.example.test");
        assert_eq!(base.destination.port(), 8080);
        assert_eq!(base.path("/v1/messages", "/messages"), "/v1/messages");
    }

    #[test]
    fn a_prefix_is_trimmed_and_gains_the_operation_suffix() {
        for (base_url, expected) in [
            ("http://h.test/v1", "/v1/messages"),
            ("http://h.test/api/v1", "/api/v1/messages"),
            ("http://h.test/v1//", "/v1/messages"),
        ] {
            let base = parse(base_url).expect("a valid URL");
            assert_eq!(
                base.path("/v1/messages", "/messages"),
                expected,
                "{base_url}"
            );
        }
    }

    #[test]
    fn an_empty_segment_or_a_part_the_parser_does_not_interpret_is_refused() {
        for base_url in [
            "http://h.test//v1",
            "http://h.test/api//v1",
            "http://user:secret@h.test",
            "http://h.test/v1?key=1",
            "http://h.test/v1#frag",
            "http://",
            "http://:8080",
            "http://h.test:0",
            "http://h.test:",
            "http://h.test:70000",
            "http://h.test:not-a-port",
            "h.test",
            "ftp://h.test",
            "http://h.test with space",
        ] {
            let error = invalid(base_url);
            assert!(
                matches!(
                    error,
                    EndpointError::BaseUrl {
                        field: "provider.base_url",
                        ..
                    }
                ),
                "`{base_url}` must be a base-URL refusal: {error:?}"
            );
            assert!(
                !format!("{error:?} {error}").contains(base_url),
                "a diagnostic must not quote the URL: {error}"
            );
        }
    }

    /// The refusal is a value the caller can log; a base URL may carry a secret in its user
    /// information, so the type must never print one.
    #[test]
    fn a_refused_url_does_not_quote_the_user_information() {
        let error = invalid("http://user:sk-test-secret@h.test");
        assert!(!format!("{error:?} {error}").contains("sk-test-secret"));
    }
}
