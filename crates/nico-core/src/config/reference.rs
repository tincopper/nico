//! Reference validation: does a route name something that exists, exactly once?
//!
//! Three decisions belong here, and only these three:
//!
//! 1. **Target shape** — every route names exactly one upstream or provider. The two mistakes get
//!    their own error ([`DocumentError::RouteTargetMissing`] /
//!    [`DocumentError::RouteTargetAmbiguous`]) instead of surfacing as a generic schema failure,
//!    and the resolved name becomes a [`RouteTarget`] so the illegal states are unrepresentable
//!    afterwards. The target kind also owns three fields: a provider route must name a
//!    [`protocol`](Route::protocol), and neither kind may set a field the other owns. That is
//!    checked here, with the target, because "which fields does this route have" is exactly the
//!    question the target answers; a value left on the wrong kind would otherwise be read as a
//!    default and silently ignored.
//! 2. **Existence and uniqueness** — route ids are globally unique, upstream, provider and plugin
//!    names are unique within their collection, and every route target resolves.
//! 3. **Plugin references** — every name a route lists is a declared plugin, and no route lists the
//!    same plugin twice. A route that names the same policy twice cannot be given a meaning (run it
//!    twice? once?) that an operator could predict, so it is rejected rather than deduplicated.
//!
//!    The same three rules govern a route's fallback chain: every member is a declared provider, no
//!    member appears twice, and the route's own target is not re-listed after itself — the chain is
//!    the order the gateway tries, and trying the target it just gave up on is not a meaning an
//!    operator could have asked for.
//!
//!    The same holds for `global_plugins`: it names declared plugins, never defines one, and lists
//!    each name once. Definitions and use sites are separate on purpose — a plugin declared at the
//!    root is a definition, and running it for every request is a decision the document has to make
//!    explicitly instead of getting it from the fact that it declared something.
//!
//! Field shapes and value ranges are [`super::semantic`]'s job. Keeping the two apart is what lets
//! a caller tell "the document points at nothing" from "the document is malformed", which is the
//! same split the validation stages expose.

use std::collections::HashSet;

use super::{
    error::DocumentError,
    plugin::PluginSpec,
    provider::Provider,
    raw::{RawPoolMember, RawRoute},
    route::{Pool, PoolMember, PoolStrategy, Route, RouteMode, RouteTarget},
    upstream::Upstream,
};

/// Inclusive upper bound for `routes[].providers[].weight`.
///
/// The same ceiling the traffic layer's member weights use, for the same reason: a larger number is
/// not a ratio an operator means, it is arithmetic they got wrong. Zero stays legal here, unlike
/// there, because a provider pool can name a member explicitly as a fallback.
const MAX_POOL_WEIGHT: u16 = 1000;

/// Resolves every route target and plugin reference, rejecting dangling and duplicate references.
///
/// The raw routes are taken by value: a target name moves into the returned [`Route`] instead of
/// being cloned, so the checked value and the published value share one allocation.
///
/// Checks run in this order, and the first violation stops validation: repeated route ids, repeated
/// upstream, provider and plugin names, then per route — in document order — the target shape, target
/// resolution, and every plugin reference.
///
/// # Errors
/// - [`DocumentError::DuplicateId`] for a repeated route id, upstream name, provider name or plugin
///   name, for a plugin a route lists twice, or for a fallback member a route lists twice (including
///   one that repeats the route's own target).
/// - [`DocumentError::RouteTargetMissing`] when a route names no target.
/// - [`DocumentError::RouteTargetAmbiguous`] when a route names both kinds of target.
/// - [`DocumentError::InvalidField`] when a provider route names no `protocol`, or when either route
///   kind sets a field its target does not own.
/// - [`DocumentError::ReferenceUnknown`] when a target or plugin name is not defined.
pub(super) fn validate(
    raw_routes: Vec<RawRoute>,
    upstreams: &[Upstream],
    providers: &[Provider],
    plugins: &[PluginSpec],
) -> Result<Vec<Route>, DocumentError> {
    reject_duplicates("route", raw_routes.iter().map(|route| route.id.as_str()))?;
    reject_duplicates("upstream", upstreams.iter().map(Upstream::name))?;
    reject_duplicates("provider", providers.iter().map(Provider::name))?;
    reject_duplicates("plugin", plugins.iter().map(PluginSpec::name))?;

    let known_upstreams: HashSet<&str> = upstreams.iter().map(Upstream::name).collect();
    let known_providers: HashSet<&str> = providers.iter().map(Provider::name).collect();
    let known_plugins: HashSet<&str> = plugins.iter().map(PluginSpec::name).collect();

    raw_routes
        .into_iter()
        .map(|raw| resolve(raw, &known_upstreams, &known_providers, &known_plugins))
        .collect()
}

/// Resolves the `global_plugins` list: every name is declared, and listed once.
///
/// The list is a use site, not a definition: it may be empty, and a declared plugin that is not
/// listed runs nowhere globally. Returning the names in document order keeps the plan's ordering
/// rule (`global_plugins` order, then phase) readable from the document alone.
///
/// # Errors
/// - [`DocumentError::ReferenceUnknown`] when a name is not declared in `plugins`.
/// - [`DocumentError::DuplicateId`] when a name is listed twice.
pub(super) fn global_plugins(
    names: Vec<String>,
    plugins: &[PluginSpec],
) -> Result<Vec<String>, DocumentError> {
    let known: HashSet<&str> = plugins.iter().map(PluginSpec::name).collect();
    let mut seen = HashSet::with_capacity(names.len());
    for name in &names {
        ensure_known(&known, "plugin", name)?;
        if !seen.insert(name.as_str()) {
            return Err(DocumentError::DuplicateId {
                kind: "global-plugin",
                id: name.clone(),
            });
        }
    }
    Ok(names)
}

/// Rejects the first name that the collection already contains.
fn reject_duplicates<'a>(
    kind: &'static str,
    names: impl Iterator<Item = &'a str>,
) -> Result<(), DocumentError> {
    let mut seen = HashSet::new();
    for name in names {
        if !seen.insert(name) {
            return Err(DocumentError::DuplicateId {
                kind,
                id: name.to_owned(),
            });
        }
    }
    Ok(())
}

/// Turns a route's two optional target names into exactly one [`RouteTarget`], checks that the name
/// is defined, and resolves the plugins the route lists.
fn resolve(
    raw: RawRoute,
    upstreams: &HashSet<&str>,
    providers: &HashSet<&str>,
    known_plugins: &HashSet<&str>,
) -> Result<Route, DocumentError> {
    let RawRoute {
        id,
        host,
        path_prefix,
        upstream,
        provider,
        providers: raw_pool,
        strategy,
        timeout_ms,
        ttft_timeout_ms,
        max_body_bytes,
        protocol,
        mode,
        max_response_bytes,
        plugins: raw_plugins,
        retry_budget,
        fallback: raw_fallback,
    } = raw;

    let (target, pool) = match (upstream, provider, raw_pool) {
        (None, None, None) => return Err(DocumentError::RouteTargetMissing { id }),
        // Two ways to name a provider target is ambiguous even when both name the same provider: the
        // routing decisions differ (one call versus a distribution), so the document must say which.
        (None, Some(_), Some(_)) => return Err(DocumentError::RouteTargetAmbiguous { id }),
        (Some(_), Some(_), _) | (Some(_), None, Some(_)) => {
            return Err(DocumentError::RouteTargetAmbiguous { id });
        }
        (Some(name), None, None) => {
            ensure_known(upstreams, "upstream", &name)?;
            // A strategy with no pool chooses among one member, so it is a value nothing reads. The
            // document is refused rather than silently ignoring it: an operator who wrote it believed
            // it changed something.
            if strategy.is_some() {
                return Err(DocumentError::StrategyWithoutPool { id });
            }
            (RouteTarget::Upstream(name), None)
        }
        (None, Some(name), None) => {
            ensure_known(providers, "provider", &name)?;
            if strategy.is_some() {
                return Err(DocumentError::StrategyWithoutPool { id });
            }
            (RouteTarget::Provider(name), None)
        }
        (None, None, Some(members)) => {
            let pool = resolve_pool(id.as_str(), members, strategy, providers)?;
            // The route's single target is the pool's first member, so every rule that already asks
            // "is this route's target named again in its fallback chain" keeps its meaning for a
            // pooled route: the head of the chain is the head of the pool.
            let first = pool
                .members()
                .first()
                .map(|member| member.name().to_owned())
                .ok_or_else(|| DocumentError::PoolEmpty { id: id.clone() })?;
            (RouteTarget::Provider(first), Some(pool))
        }
    };

    check_target_fields(
        &target,
        protocol.as_deref(),
        mode,
        max_response_bytes,
        retry_budget.is_some(),
        raw_fallback.is_some(),
        ttft_timeout_ms.is_some(),
    )?;

    let plugins = raw_plugins.unwrap_or_default();
    let mut seen = HashSet::with_capacity(plugins.len());
    for name in &plugins {
        ensure_known(known_plugins, "plugin", name)?;
        if !seen.insert(name.as_str()) {
            return Err(DocumentError::DuplicateId {
                kind: "route-plugin",
                id: name.clone(),
            });
        }
    }

    // A fallback chain is a use site, not a definition: every member is a declared provider, and the
    // chain visits each one at most once. The route's own target is the chain's head by construction,
    // so re-listing it would try the provider the route just gave up on. An upstream route owns no
    // chain at all — `check_target_fields` refused a written one above, so this list is empty.
    let fallback = raw_fallback.unwrap_or_default();
    if let RouteTarget::Provider(primary) = &target {
        let mut chain = HashSet::with_capacity(fallback.len());
        for name in &fallback {
            ensure_known(providers, "provider", name)?;
            if *name == primary.as_str() || !chain.insert(name.as_str()) {
                return Err(DocumentError::DuplicateId {
                    kind: "route-fallback",
                    id: name.clone(),
                });
            }
        }
    }

    Ok(Route {
        id,
        host,
        path_prefix,
        target,
        timeout_ms,
        ttft_timeout_ms,
        max_body_bytes,
        protocol,
        mode: mode.unwrap_or_default(),
        max_response_bytes,
        plugins,
        retry_budget: retry_budget.unwrap_or(1),
        fallback,
        pool,
    })
}

/// Resolves one route's `providers` list into a pool.
///
/// Every member is a declared provider, listed once, and inside the configured member bound. The
/// strategy is the document's or the default; a `strategy` on a route without a pool is refused,
/// because a value no code path reads is a typo rather than configuration.
fn resolve_pool(
    route: &str,
    raw: Vec<RawPoolMember>,
    strategy: Option<PoolStrategy>,
    providers: &HashSet<&str>,
) -> Result<Pool, DocumentError> {
    if raw.is_empty() {
        return Err(DocumentError::PoolEmpty {
            id: route.to_owned(),
        });
    }
    let mut seen = HashSet::with_capacity(raw.len());
    let mut members = Vec::with_capacity(raw.len());
    for member in raw {
        ensure_known(providers, "provider", &member.name)?;
        if !seen.insert(member.name.clone()) {
            return Err(DocumentError::DuplicateId {
                kind: "route-pool-member",
                id: member.name,
            });
        }
        if member.weight.is_some_and(|weight| weight > MAX_POOL_WEIGHT) {
            return Err(DocumentError::LimitOutOfRange {
                field: "routes[].providers[].weight",
                value: u64::from(member.weight.unwrap_or_default()),
            });
        }
        members.push(PoolMember::new(member.name, member.weight.unwrap_or(1)));
    }
    Ok(Pool::new(members, strategy.unwrap_or_default()))
}

/// Rejects a field that the route's target kind does not own, and a provider route without a
/// protocol.
///
/// The check is per kind rather than per field list so the error names the field the operator wrote:
/// an upstream route has nothing to convert, so `protocol`, `mode` and `max_response_bytes` would
/// otherwise be read as defaults and silently dropped, and a provider route has nothing to route on
/// without the name of the client protocol. The attempt fields — `retry_budget`, `fallback` — belong
/// to the same converted exchange: they decide how a *classified* failure is retried, and an upstream
/// route forwards bytes without ever classifying one.
fn check_target_fields(
    target: &RouteTarget,
    protocol: Option<&str>,
    mode: Option<RouteMode>,
    max_response_bytes: Option<u32>,
    retry_budget: bool,
    fallback: bool,
    ttft_timeout: bool,
) -> Result<(), DocumentError> {
    match target {
        RouteTarget::Upstream(_) => {
            let written = [
                ("routes[].protocol", protocol.is_some()),
                ("routes[].mode", mode.is_some()),
                ("routes[].max_response_bytes", max_response_bytes.is_some()),
                ("routes[].retry_budget", retry_budget),
                ("routes[].fallback", fallback),
                ("routes[].ttft_timeout_ms", ttft_timeout),
            ];
            for (field, present) in written {
                if present {
                    return Err(DocumentError::InvalidField {
                        field,
                        reason: "is only valid on a provider route".into(),
                    });
                }
            }
        }
        RouteTarget::Provider(_) => {
            if protocol.is_none() {
                return Err(DocumentError::InvalidField {
                    field: "routes[].protocol",
                    reason: "is required on a provider route".into(),
                });
            }
        }
    }
    Ok(())
}

/// Rejects a target or plugin name that the collection does not define.
fn ensure_known(
    known: &HashSet<&str>,
    kind: &'static str,
    name: &str,
) -> Result<(), DocumentError> {
    if known.contains(name) {
        return Ok(());
    }
    Err(DocumentError::ReferenceUnknown {
        kind,
        name: name.to_owned(),
    })
}

#[cfg(test)]
mod tests;
