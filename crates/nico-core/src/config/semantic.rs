//! Semantic validation: field shapes and value ranges of a document that already resolves.
//!
//! Every rule here is local to one entry — a name, a host, a path prefix, a port, a timeout — so a
//! failure names the field it came from instead of the reference that led there. Whether a target
//! exists, whether ids are unique, and which fields a target kind owns is [`super::reference`]'s job.
//!
//! `Scheme` and `RouteMode` are not re-validated here: they are closed enums whose serde
//! implementation rejects an unknown spelling while the document is decoded, so "unknown scheme" is
//! reported as a schema error before this stage runs.
//!
//! `path_prefix` is checked for syntax only. Whether `/api/` is equivalent to a directory prefix,
//! and how two matching prefixes are ranked, is match semantics and belongs to the data plane.
//!
//! A `protocol` and a provider `kind` are checked for shape only. Which of either exists is known to
//! the registry that compiles them at publication, so an unknown name is that registry's rejection,
//! not a document error: this crate would have to learn every vendor to answer the question here, and
//! adding a provider would then be a change to the kernel.

use std::{
    borrow::Cow,
    collections::{BTreeMap, HashSet},
};

use super::{
    error::DocumentError,
    host,
    plugin::PluginSpec,
    provider::{CircuitBreaker, Provider},
    request::RequestBounds,
    route::Route,
    secret_ref::is_secret_ref,
    upstream::Upstream,
};

/// Inclusive upper bound for a route-level or provider-level `timeout_ms`.
const MAX_TIMEOUT_MS: u32 = 600_000;

/// Inclusive upper bound for `routes[].ttft_timeout_ms`.
///
/// Half the whole-request ceiling on purpose: this budget bounds the wait *before* an answer starts,
/// so a value approaching the request's own deadline would never fire — the request would time out
/// first, and the field would look like configuration that does nothing.
const MAX_TTFT_TIMEOUT_MS: u32 = 300_000;
/// Inclusive upper bound for `routes[].retry_budget`.
///
/// A retry re-encodes the canonical request, so the budget bounds the memory a route holds in flight
/// and the load one failing endpoint absorbs. Four attempts is already an aggressive posture for a
/// call that takes seconds; a larger value is a deployment problem, not a knob.
pub(crate) const MAX_RETRY_BUDGET: u8 = 4;
/// Inclusive upper bound for the members of `routes[].fallback`.
///
/// A chain is a declared compatibility statement, not a failover list: three alternatives after the
/// route's own target is already more endpoints than one request should be allowed to try, and every
/// member is a credential the gateway materializes and a place its prompt may be sent.
pub const MAX_FALLBACK_MEMBERS: usize = 3;
/// The most attempts one route can declare, which is what the data plane's loop fuse must cover.
///
/// Derived rather than written down, because three independent bounds multiply: a budget retries one
/// member, a pool adds members each of which the budget applies to, and a chain adds a target per
/// member. The longest sequence a document can express is therefore the product below. The data
/// plane reads this constant as `ServerConf::max_retries`, which makes the framework's counter and
/// the document model one fact instead of two numbers kept in agreement by hand.
///
/// The pool factor is the built-in bound. A document validated against a raised `max_pool_members`
/// gets its ceiling from [`attempt_ceiling`] instead, because a fuse sized for the default would
/// truncate that document's legitimate sequence.
pub const MAX_ATTEMPTS_PER_ROUTE: usize =
    MAX_RETRY_BUDGET as usize * DEFAULT_MAX_POOL_MEMBERS * (1 + MAX_FALLBACK_MEMBERS);

/// The most attempts one route of a document validated against `schema` can declare.
///
/// The fuse a data plane installs must cover the document it actually validated, not the default
/// shape.
#[must_use]
pub const fn attempt_ceiling(schema: super::SchemaBounds) -> usize {
    MAX_RETRY_BUDGET as usize * schema.get(super::Bound::PoolMembers) * (1 + MAX_FALLBACK_MEMBERS)
}

/// The built-in pool member bound [`MAX_ATTEMPTS_PER_ROUTE`] is derived from.
///
/// Named here so the constant cannot disagree with [`SchemaBounds::default`](super::SchemaBounds):
/// the bounds module owns the number, this one derives from it.
const DEFAULT_MAX_POOL_MEMBERS: usize = 8;
/// Inclusive upper bound for `providers[].circuit_breaker.failure_threshold`.
///
/// The threshold counts consecutive failures, so a document writing a larger one has not configured
/// a breaker but expressed a number; `0` stays legal and means the breaker is disabled.
const MAX_FAILURE_THRESHOLD: u32 = 1000;
/// Inclusive upper bound for `providers[].circuit_breaker.cooldown_ms`.
///
/// The same ceiling a timeout uses: a cooldown past ten minutes is a maintenance window, which an
/// operator expresses by disabling the breaker, not by waiting an hour between probes.
const MAX_COOLDOWN_MS: u32 = 600_000;
/// Upper bound for a route id, an upstream name or a provider name.
pub(super) const MAX_NAME_BYTES: usize = 63;
/// Upper bound for `routes[].path_prefix`.
const MAX_PATH_PREFIX_BYTES: usize = 256;
/// Upper bound for `routes[].protocol`.
const MAX_PROTOCOL_BYTES: usize = 64;
/// Upper bound for `providers[].kind`.
const MAX_PROVIDER_KIND_BYTES: usize = 64;
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
///   reference, upstream host or port, protocol id or model map entry.
/// - [`DocumentError::LimitOutOfRange`] for a timeout outside `1..=600_000`, an output ceiling of
///   zero, a route body bound above `request.max_body_bytes`, a route response bound above
///   `request.max_response_bytes`, a retry budget outside `1..=4`, a breaker threshold above its
///   ceiling, a breaker cooldown outside `1..=600_000`, an upstream `max_fails` above its ceiling or
///   an upstream `fail_timeout_ms` outside `1..=600_000`.
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
        upstreams::validate(upstream)?;
    }
    plugins::validate(plugins)?;
    validate_routes(routes, request)
}

/// Checks one provider: name, kind, base URL, credential reference, model map, output ceiling and
/// optional timeout.
fn validate_provider(provider: &Provider) -> Result<(), DocumentError> {
    validate_name("providers[].name", provider.name())?;
    validate_token("providers[].kind", provider.kind(), MAX_PROVIDER_KIND_BYTES)?;

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
    if provider.max_output_tokens() == Some(0) {
        return Err(DocumentError::LimitOutOfRange {
            field: "providers[].max_output_tokens",
            value: 0,
        });
    }
    validate_circuit_breaker(provider.circuit_breaker())?;
    validate_model_map(provider.model_map())?;
    Ok(())
}

/// Checks one provider's circuit-breaker parameters.
///
/// `failure_threshold` of `0` is legal and means the breaker is disabled — it is the one spelling for
/// "never stop calling this endpoint", which is why the range starts there and not at one.
fn validate_circuit_breaker(breaker: &CircuitBreaker) -> Result<(), DocumentError> {
    let threshold = breaker.failure_threshold();
    if threshold > MAX_FAILURE_THRESHOLD {
        return Err(DocumentError::LimitOutOfRange {
            field: "providers[].circuit_breaker.failure_threshold",
            value: u64::from(threshold),
        });
    }
    let cooldown = breaker.cooldown_ms();
    if !(1..=MAX_COOLDOWN_MS).contains(&cooldown) {
        return Err(DocumentError::LimitOutOfRange {
            field: "providers[].circuit_breaker.cooldown_ms",
            value: u64::from(cooldown),
        });
    }
    Ok(())
}

/// Checks a logical → physical model map: both sides of every entry must be a name a caller can
/// spell.
///
/// A model name travels in a URL, a header or a JSON string depending on the vendor, so whitespace
/// and control characters are refused rather than escaped. Nothing beyond that is imposed: a vendor's
/// naming scheme is the vendor's, and the same holds for `model_allowlist`, which is deliberately
/// not narrowed here. The message names the side that broke, never the value: a mapping value is
/// endpoint configuration, and an invalid one is usually a copy-paste accident worth fixing without
/// being echoed into a log.
fn validate_model_map(map: &BTreeMap<String, String>) -> Result<(), DocumentError> {
    const FIELD: &str = "providers[].model_map";
    for (logical, physical) in map {
        for (side, name) in [("keys", logical), ("values", physical)] {
            if name.is_empty() {
                return Err(invalid(FIELD, format!("{side} must not be empty")));
            }
            if name
                .chars()
                .any(|character| character.is_whitespace() || character.is_control())
            {
                return Err(invalid(
                    FIELD,
                    format!("{side} must not contain whitespace or control characters"),
                ));
            }
        }
    }
    Ok(())
}

/// Checks every route and rejects a repeated `(host, path_prefix)` match combination.
fn validate_routes(routes: &[Route], request: RequestBounds) -> Result<(), DocumentError> {
    let mut matches: HashSet<(Option<String>, &str)> = HashSet::new();

    for route in routes {
        validate_name("routes[].id", route.id())?;
        if let Some(host) = route.host() {
            validate_host("routes[].host", host)?;
        }
        validate_path_prefix(route.path_prefix())?;

        if let Some(timeout_ms) = route.timeout_ms() {
            validate_timeout("routes[].timeout_ms", timeout_ms)?;
        }
        if let Some(ttft_timeout_ms) = route.ttft_timeout_ms() {
            validate_timeout_bound(
                "routes[].ttft_timeout_ms",
                ttft_timeout_ms,
                MAX_TTFT_TIMEOUT_MS,
            )?;
        }
        if let Some(max_body_bytes) = route.max_body_bytes() {
            validate_body_bound(request, max_body_bytes)?;
        }
        if let Some(protocol) = route.protocol() {
            validate_token("routes[].protocol", protocol, MAX_PROTOCOL_BYTES)?;
        }
        if let Some(max_response_bytes) = route.max_response_bytes() {
            validate_response_bound(request, max_response_bytes)?;
        }
        validate_retry_budget(route.retry_budget())?;
        validate_fallback_chain(route)?;

        // The key is the router's own comparison, canonicalised: `host_key` and `hosts_match` are one
        // rule, so a document cannot list `API.example` and `api.example` as two routes while the
        // router serves them as one.
        let key = (route.host().map(host::host_key), route.path_prefix());
        if !matches.insert(key) {
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
    validate_timeout_bound(field, timeout_ms, MAX_TIMEOUT_MS)
}

/// Checks a timeout stays inside its own ceiling.
///
/// Split from [`validate_timeout`] because the two ceilings differ: a whole-request budget and a
/// first-byte budget are not the same posture, and one constant for both would let a document state a
/// first-byte wait longer than the request it belongs to.
fn validate_timeout_bound(
    field: &'static str,
    timeout_ms: u32,
    maximum: u32,
) -> Result<(), DocumentError> {
    if !(1..=maximum).contains(&timeout_ms) {
        return Err(DocumentError::LimitOutOfRange {
            field,
            value: u64::from(timeout_ms),
        });
    }
    Ok(())
}

/// Checks a route body bound stays within the document bound.
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

/// Checks a route response bound stays within the document bound.
///
/// The same rule as [`validate_body_bound`], for the same reason: the document-wide value is the
/// budget an operator sized for the process, so a route may narrow its exposure and never widen it.
fn validate_response_bound(
    request: RequestBounds,
    max_response_bytes: u32,
) -> Result<(), DocumentError> {
    if max_response_bytes > request.max_response_bytes() {
        return Err(DocumentError::LimitOutOfRange {
            field: "routes[].max_response_bytes",
            value: u64::from(max_response_bytes),
        });
    }
    Ok(())
}

/// Checks a route's retry budget.
///
/// A budget of `0` would mean "no attempts at all", which no route can serve, so the range starts at
/// the default `1`. The upper bound is the document model's, not a request-time decision: each retry
/// holds another encoded body in flight, so the ceiling is a memory posture.
fn validate_retry_budget(retry_budget: u8) -> Result<(), DocumentError> {
    if !(1..=MAX_RETRY_BUDGET).contains(&retry_budget) {
        return Err(DocumentError::LimitOutOfRange {
            field: "routes[].retry_budget",
            value: u64::from(retry_budget),
        });
    }
    Ok(())
}

/// Checks a route's fallback chain length.
///
/// The bound is the document model's because it is a memory and blast-radius posture: every member is
/// another target one request may reach, with its own credential and its own copy of the encoded body
/// in flight. It is also the second factor of [`MAX_ATTEMPTS_PER_ROUTE`], so a document cannot express
/// a sequence the data plane's attempt loop is not sized for.
fn validate_fallback_chain(route: &Route) -> Result<(), DocumentError> {
    if route.fallback().len() > MAX_FALLBACK_MEMBERS {
        return Err(DocumentError::LimitOutOfRange {
            field: "routes[].fallback",
            value: route.fallback().len() as u64,
        });
    }
    Ok(())
}

/// Checks a protocol id: printable ASCII, no spaces, at most 64 bytes.
///
/// It becomes a registry lookup key and a log field, so a control character or a space would either
/// address nothing or split the line it is written into. The shape is the whole rule: which ids
/// exist is decided by the codec registry at publication.
fn validate_token(field: &'static str, token: &str, limit: usize) -> Result<(), DocumentError> {
    let printable = token.bytes().all(|byte| byte.is_ascii_graphic());
    if token.is_empty() || token.len() > limit || !printable {
        return Err(invalid(
            field,
            format!("must be 1..={limit} printable ASCII bytes with no spaces"),
        ));
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
mod upstreams;

#[cfg(test)]
mod tests;
