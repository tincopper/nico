//! Semantic validation: field shapes and value ranges of a document that already resolves.
//!
//! Every rule here is local to one entry — a name, a host, a path prefix, a port, a timeout — so a
//! failure names the field it came from instead of the reference that led there. Whether a target
//! exists and whether ids are unique is [`super::reference`]'s job.
//!
//! `Scheme` and `ProviderKind` are not re-validated here: they are closed enums whose serde
//! implementation rejects an unknown spelling while the document is decoded, so "unknown scheme" is
//! reported as a schema error before this stage runs.
//!
//! `path_prefix` is checked for syntax only. Whether `/api/` is equivalent to a directory prefix,
//! and how two matching prefixes are ranked, is match semantics and belongs to the data plane.

use std::{borrow::Cow, collections::HashSet};

use super::{
    error::DocumentError, host, plugin::PluginSpec, provider::Provider, request::RequestBounds,
    route::Route, secret_ref::is_secret_ref, upstream::Upstream,
};

/// Inclusive upper bound for a route-level or provider-level `timeout_ms`.
const MAX_TIMEOUT_MS: u32 = 600_000;
/// Upper bound for a route id, an upstream name or a provider name.
const MAX_NAME_BYTES: usize = 63;
/// Upper bound for `routes[].path_prefix`.
const MAX_PATH_PREFIX_BYTES: usize = 256;
/// Upper bound for a rejection `content-type`.
const MAX_CONTENT_TYPE_BYTES: usize = 128;
/// Inclusive bounds of a configurable rejection status.
///
/// 4xx only: a rejection is the caller's fault, so a 5xx would report the caller's mistake as a
/// gateway failure, and a 2xx would not be a rejection at all. It is shared by every entry that can
/// configure a rejection, because "a refusal is the caller's fault" does not depend on the kind.
const REJECTION_STATUS: std::ops::RangeInclusive<u16> = 400..=499;

/// Validates every provider, then every upstream, then every route.
///
/// Providers and upstreams are checked first because a route's validity depends on the shape of the
/// entries it points at: a bad provider name should be reported even when no route uses it yet.
///
/// # Errors
/// - [`DocumentError::InvalidField`] for a malformed name, host, path prefix, base URL, credential
///   reference, upstream host or port.
/// - [`DocumentError::LimitOutOfRange`] for a timeout outside `1..=600_000` or a route body bound
///   above `request.max_body_bytes`.
/// - [`DocumentError::DuplicateId`] when two routes share the same `(host, path_prefix)` pair.
pub(super) fn validate(
    routes: &[Route],
    upstreams: &[Upstream],
    providers: &[Provider],
    plugins: &[PluginSpec],
    request: RequestBounds,
) -> Result<(), DocumentError> {
    for provider in providers {
        validate_provider(provider)?;
    }
    for upstream in upstreams {
        validate_upstream(upstream)?;
    }
    plugins::validate(plugins)?;
    validate_routes(routes, request)
}

/// Checks one provider: name, base URL, credential reference and optional timeout.
fn validate_provider(provider: &Provider) -> Result<(), DocumentError> {
    validate_name("providers[].name", provider.name())?;

    const BASE_URL: &str = "providers[].base_url";
    if !(provider.base_url().starts_with("http://") || provider.base_url().starts_with("https://"))
    {
        return Err(invalid(BASE_URL, "must start with `http://` or `https://`"));
    }
    if has_whitespace(provider.base_url()) {
        return Err(invalid(BASE_URL, "must not contain whitespace"));
    }

    const CREDENTIAL_REF: &str = "providers[].credential_ref";
    if !is_secret_ref(provider.credential_ref()) {
        return Err(invalid(
            CREDENTIAL_REF,
            "must be an `env:` or `file:` reference naming a variable or path without control characters",
        ));
    }

    if let Some(timeout_ms) = provider.timeout_ms() {
        validate_timeout("providers[].timeout_ms", timeout_ms)?;
    }
    Ok(())
}

/// Checks one upstream: name, host and port.
///
/// The scheme needs no check here: [`Scheme`](super::Scheme) can only hold a known value.
fn validate_upstream(upstream: &Upstream) -> Result<(), DocumentError> {
    validate_name("upstreams[].name", upstream.name())?;
    validate_host("upstreams[].host", upstream.host())?;
    if upstream.port() == 0 {
        return Err(invalid("upstreams[].port", "must be in 1..=65535"));
    }
    Ok(())
}

/// Checks every route and rejects a repeated `(host, path_prefix)` match combination.
fn validate_routes(routes: &[Route], request: RequestBounds) -> Result<(), DocumentError> {
    let mut matches: HashSet<(Option<&str>, &str)> = HashSet::new();

    for route in routes {
        validate_name("routes[].id", route.id())?;
        if let Some(host) = route.host() {
            validate_host("routes[].host", host)?;
        }
        validate_path_prefix(route.path_prefix())?;

        if let Some(timeout_ms) = route.timeout_ms() {
            validate_timeout("routes[].timeout_ms", timeout_ms)?;
        }
        if let Some(max_body_bytes) = route.max_body_bytes() {
            validate_body_bound(request, max_body_bytes)?;
        }

        if !matches.insert((route.host(), route.path_prefix())) {
            // A route without a host matches any host; `*` spells that in the reported id.
            return Err(DocumentError::DuplicateId {
                kind: "route-match",
                id: format!("{}{}", route.host().unwrap_or("*"), route.path_prefix()),
            });
        }
    }
    Ok(())
}

/// Checks a route id, upstream name or provider name.
fn validate_name(field: &'static str, name: &str) -> Result<(), DocumentError> {
    if is_name(name) {
        return Ok(());
    }
    Err(invalid(
        field,
        format!("must be lowercase kebab-case and at most {MAX_NAME_BYTES} bytes"),
    ))
}

/// Returns whether `name` matches `[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?`.
fn is_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    let (Some(first), Some(last)) = (bytes.first(), bytes.last()) else {
        return false;
    };
    bytes.len() <= MAX_NAME_BYTES
        && is_lowercase_alphanumeric(*first)
        && is_lowercase_alphanumeric(*last)
        && bytes
            .iter()
            .all(|&byte| is_lowercase_alphanumeric(byte) || byte == b'-')
}

/// Returns whether `byte` is `a-z` or `0-9`.
const fn is_lowercase_alphanumeric(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit()
}

/// Checks a host name, naming the field that broke.
///
/// The rule itself is [`host::validate_host`]'s, so the document model and an adapter that derives a
/// destination from a base URL cannot disagree about what a host may be. `field` names the one that
/// broke.
fn validate_host(field: &'static str, host: &str) -> Result<(), DocumentError> {
    host::validate_host(host).map_err(|problem| invalid(field, problem.reason()))
}

/// Checks a path prefix: rooted, no parent segment, no whitespace, at most 256 bytes.
fn validate_path_prefix(prefix: &str) -> Result<(), DocumentError> {
    const FIELD: &str = "routes[].path_prefix";
    if !prefix.starts_with('/') {
        return Err(invalid(FIELD, "must start with `/`"));
    }
    if prefix.contains("..") {
        return Err(invalid(FIELD, "must not contain `..`"));
    }
    if has_whitespace(prefix) {
        return Err(invalid(FIELD, "must not contain whitespace"));
    }
    if prefix.len() > MAX_PATH_PREFIX_BYTES {
        return Err(invalid(
            FIELD,
            format!("must be at most {MAX_PATH_PREFIX_BYTES} bytes"),
        ));
    }
    Ok(())
}

/// Checks a route-level or provider-level timeout.
fn validate_timeout(field: &'static str, timeout_ms: u32) -> Result<(), DocumentError> {
    if !(1..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
        return Err(DocumentError::LimitOutOfRange {
            field,
            value: u64::from(timeout_ms),
        });
    }
    Ok(())
}

/// Checks that a route body bound stays within the document bound.
///
/// A route may only tighten: `0` is legal (the route accepts no request body at all) and equals what
/// the document bound means at `0`, but a route can never widen the document ceiling.
fn validate_body_bound(request: RequestBounds, max_body_bytes: u32) -> Result<(), DocumentError> {
    if max_body_bytes > request.max_body_bytes() {
        return Err(DocumentError::LimitOutOfRange {
            field: "routes[].max_body_bytes",
            value: u64::from(max_body_bytes),
        });
    }
    Ok(())
}

/// Checks a rejection content type: printable, and a `type/subtype` pair.
///
/// It becomes a response header, so a control character here would let a document split the header
/// it is written into. The `;` parameter form (`application/json; charset=utf-8`) is accepted.
fn validate_content_type(field: &'static str, value: &str) -> Result<(), DocumentError> {
    if value.is_empty() {
        return Err(invalid(field, "must not be empty"));
    }
    if value.len() > MAX_CONTENT_TYPE_BYTES {
        return Err(invalid(
            field,
            format!("must be at most {MAX_CONTENT_TYPE_BYTES} bytes"),
        ));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_graphic() || byte == b' ')
        || !value.contains('/')
    {
        return Err(invalid(field, "must be a `type/subtype` string"));
    }
    Ok(())
}

/// Returns whether `value` contains any whitespace.
fn has_whitespace(value: &str) -> bool {
    value.chars().any(char::is_whitespace)
}

/// Builds an [`DocumentError::InvalidField`] for a rejected value.
///
/// `reason` is taken by `Into<Cow>`: a rule that quotes a limit formats it from the constant the
/// check used, so the message and the check cannot disagree.
fn invalid(field: &'static str, reason: impl Into<Cow<'static, str>>) -> DocumentError {
    DocumentError::InvalidField {
        field,
        reason: reason.into(),
    }
}

mod plugins;

#[cfg(test)]
mod tests;
