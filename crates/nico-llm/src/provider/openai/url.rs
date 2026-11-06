//! Reading a route's `base_url` as a destination and the chat endpoint's path.
//!
//! A base URL is an *API prefix*, not a request target: a route names a provider and its prefix, and
//! the operation is fixed by the adapter, because [`Endpoint`](super::Endpoint) may be resolved
//! before the request body is decoded. The rules are therefore closed:
//!
//! - `http://host` or `https://host` (scheme in any case, default ports 80 and 443) names an API
//!   root, and the chat path is `/v1/chat/completions`.
//! - Any base path is a prefix: `.../v1` yields `/v1/chat/completions`, and `.../openai` yields
//!   `/openai/chat/completions`. Trailing slashes are ignored, and a prefix that still carries an
//!   empty segment (`.../openai//v1`) is refused rather than sent as a request target whose meaning
//!   the origin would have to decide — a leading `//` reads as a network-path reference to some
//!   peers.
//! - Every part of a URL this adapter does not interpret is refused rather than dropped: user
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
//! relative references) that this adapter would then have to re-refuse.

use std::borrow::Cow;

use nico_core::{Scheme, validate_host};
use nico_http::Destination;

use super::Endpoint;
use crate::provider::{EndpointError, ProviderRoute};

/// The chat path of a base URL that names no path of its own.
const DEFAULT_REQUEST_PATH: &str = "/v1/chat/completions";

/// The path appended to a base URL that names an API prefix.
const CHAT_PATH: &str = "/chat/completions";

/// The dotted configuration field every base-URL failure names.
const FIELD: &str = "provider.base_url";

/// Builds the endpoint the data plane will contact for one route.
///
/// # Errors
/// Returns [`EndpointError::BaseUrl`] when the route's `base_url` is not an addressable URL, and the
/// [`Endpoint`] checks' own variants when a derived path or header is not sendable.
pub(super) fn endpoint(route: &ProviderRoute) -> Result<Endpoint, EndpointError> {
    let base = parse(route.base_url())?;
    Endpoint::try_new(base.destination, chat_path(&base.path)?, headers(route))
}

/// One parsed base URL: where to connect and the path prefix to keep.
struct Base {
    destination: Destination,
    path: String,
}

/// Parses the `scheme://authority/path` form this adapter accepts.
fn parse(base_url: &str) -> Result<Base, EndpointError> {
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
        path: path.to_owned(),
    })
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

/// Builds the chat path from a base path prefix.
///
/// The join itself never produces `//`, because the trailing slashes are dropped first. A prefix
/// that carries an empty segment of its own is refused: the adapter will not hand an origin a path
/// it would have to reinterpret, and a leading `//` is read as a network-path reference by some
/// peers.
fn chat_path(base_path: &str) -> Result<String, EndpointError> {
    let prefix = base_path.trim_end_matches('/');
    if prefix.is_empty() {
        return Ok(DEFAULT_REQUEST_PATH.to_owned());
    }
    if prefix.contains("//") {
        return Err(fail("the path carries an empty segment"));
    }
    Ok(format!("{prefix}{CHAT_PATH}"))
}

/// Builds the headers the provider adds to the request.
///
/// `accept` names both shapes one endpoint answers: the same path returns a complete document or an
/// event stream depending on a field of the request body, and the endpoint may not read the body, so
/// the adapter advertises both instead of guessing.
fn headers(route: &ProviderRoute) -> Vec<(String, String)> {
    let mut headers = vec![
        ("content-type".to_owned(), "application/json".to_owned()),
        (
            "accept".to_owned(),
            "application/json, text/event-stream".to_owned(),
        ),
    ];
    if let Some(credential) = route.credential() {
        headers.push((
            "authorization".to_owned(),
            format!("Bearer {}", credential.expose()),
        ));
    }
    headers
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

/// Builds the one base-URL failure this module reports.
fn fail(reason: impl Into<Cow<'static, str>>) -> EndpointError {
    EndpointError::BaseUrl {
        field: FIELD,
        reason: reason.into(),
    }
}
