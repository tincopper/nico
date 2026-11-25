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

mod pools;
mod target;

use std::collections::HashSet;

use self::{
    pools::{resolve_pool, resolve_upstream_pool},
    target::{WrittenFields, check_target_fields},
};
use super::{
    error::DocumentError,
    plugin::PluginSpec,
    provider::Provider,
    raw::RawRoute,
    route::{ProxyHeaderValue, Route, RouteTarget, new_proxy_set_header, new_token_budget},
    upstream::Upstream,
};

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
        upstreams: raw_upstream_pool,
        rewrite_prefix: raw_rewrite,
        proxy_buffering,
        max_buffer_size: raw_max_buffer,
        proxy_set_header: raw_set_headers,
        proxy_del_header: raw_del_headers,
        strategy,
        timeout_ms,
        ttft_timeout_ms,
        token_budget: raw_token_budget,

        max_body_bytes,
        protocol,
        mode,
        max_response_bytes,
        plugins: raw_plugins,
        retry_budget,
        fallback: raw_fallback,
    } = raw;

    // Exactly one way to name a target: a single upstream, a single provider, a provider pool, or an
    // upstream pool. Two at once is ambiguous even when both name the same service — the routing
    // decisions differ (one target versus a distribution) — and none is a route with no target.
    let named = usize::from(upstream.is_some())
        + usize::from(provider.is_some())
        + usize::from(raw_pool.is_some())
        + usize::from(raw_upstream_pool.is_some());
    if named == 0 {
        return Err(DocumentError::RouteTargetMissing { id });
    }
    if named > 1 {
        return Err(DocumentError::RouteTargetAmbiguous { id });
    }

    let (target, pool, upstream_pool) = if let Some(name) = upstream {
        ensure_known(upstreams, "upstream", &name)?;
        // A strategy with no pool chooses among one member, so it is a value nothing reads. The
        // document is refused rather than silently ignoring it: an operator who wrote it believed
        // it changed something.
        if strategy.is_some() {
            return Err(DocumentError::StrategyWithoutPool { id });
        }
        (RouteTarget::Upstream(name), None, None)
    } else if let Some(name) = provider {
        ensure_known(providers, "provider", &name)?;
        if strategy.is_some() {
            return Err(DocumentError::StrategyWithoutPool { id });
        }
        (RouteTarget::Provider(name), None, None)
    } else if let Some(members) = raw_pool {
        let pool = resolve_pool(id.as_str(), members, strategy, providers)?;
        // The route's single target is the pool's first member, so every rule that already asks
        // "is this route's target named again in its fallback chain" keeps its meaning for a
        // pooled route: the head of the chain is the head of the pool.
        let first = pool
            .members()
            .first()
            .map(|member| member.name().to_owned())
            .ok_or_else(|| DocumentError::PoolEmpty { id: id.clone() })?;
        (RouteTarget::Provider(first), Some(pool), None)
    } else {
        // The count check left this as the one naming form; `unwrap_or_default` only silences the
        // impossible `None`, and an empty list reaches `resolve_upstream_pool`'s own refusal.
        let members = raw_upstream_pool.unwrap_or_default();
        let upstream_pool = resolve_upstream_pool(id.as_str(), members, strategy, upstreams)?;
        // The same first-member rule as a provider pool: the route's target is the pool's head, so
        // rules that read a target keep their meaning.
        let first = upstream_pool
            .members()
            .first()
            .map(|member| member.upstream().to_owned())
            .ok_or_else(|| DocumentError::PoolEmpty { id: id.clone() })?;
        (RouteTarget::Upstream(first), None, Some(upstream_pool))
    };

    // The proxy-behavior fields are upstream-route vocabulary: a converting route's request and
    // response belong to its adapter, and `check_target_fields` refuses all five on one.
    check_target_fields(
        &target,
        WrittenFields {
            proxy_buffering: proxy_buffering.is_some(),
            max_buffer_size: raw_max_buffer.is_some(),
            proxy_set_header: raw_set_headers.is_some(),
            proxy_del_header: raw_del_headers.is_some(),
            rewrite_prefix: raw_rewrite.is_some(),
            protocol: protocol.as_deref(),
            mode,
            max_response_bytes,
            fallback: raw_fallback.is_some(),
            ttft_timeout: ttft_timeout_ms.is_some(),
            token_budget: raw_token_budget.is_some(),
        },
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

    // The proxy-behavior defaults are nginx's: both buffering switches on, a 1 MiB response spool.
    // An upstream-only route has them; the target check above refused a provider route that wrote
    // any of them, so a provider route takes the same defaults and the fields stay unused there.
    const DEFAULT_MAX_BUFFER_SIZE: u32 = 1024 * 1024;
    let set_headers = raw_set_headers
        .unwrap_or_default()
        .into_iter()
        .map(|entry| {
            let value = match entry.value.as_str() {
                "client_host" => ProxyHeaderValue::ClientHost,
                "upstream_host" => ProxyHeaderValue::UpstreamHost,
                literal => ProxyHeaderValue::Literal(literal.to_owned()),
            };
            new_proxy_set_header(entry.name, value)
        })
        .collect();
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
        token_budget: raw_token_budget
            .map(|raw| new_token_budget(raw.limit_tokens, raw.window_seconds)),
        pool,
        upstream_pool,
        rewrite_prefix: raw_rewrite,
        proxy_buffering: proxy_buffering.unwrap_or(true),
        max_buffer_size: raw_max_buffer.unwrap_or(DEFAULT_MAX_BUFFER_SIZE),
        proxy_set_header: set_headers,
        proxy_del_header: raw_del_headers.unwrap_or_default(),
    })
}

/// Rejects a target or plugin name that the collection does not define.
pub(super) fn ensure_known(
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
