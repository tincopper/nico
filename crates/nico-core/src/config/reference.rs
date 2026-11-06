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
    raw::RawRoute,
    route::{Route, RouteMode, RouteTarget},
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
///   name, or for a plugin a route lists twice.
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
        timeout_ms,
        max_body_bytes,
        protocol,
        mode,
        max_response_bytes,
        plugins: raw_plugins,
    } = raw;

    let target = match (upstream, provider) {
        (None, None) => return Err(DocumentError::RouteTargetMissing { id }),
        (Some(_), Some(_)) => return Err(DocumentError::RouteTargetAmbiguous { id }),
        (Some(name), None) => {
            ensure_known(upstreams, "upstream", &name)?;
            RouteTarget::Upstream(name)
        }
        (None, Some(name)) => {
            ensure_known(providers, "provider", &name)?;
            RouteTarget::Provider(name)
        }
    };

    check_target_fields(&target, protocol.as_deref(), mode, max_response_bytes)?;

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

    Ok(Route {
        id,
        host,
        path_prefix,
        target,
        timeout_ms,
        max_body_bytes,
        protocol,
        mode: mode.unwrap_or_default(),
        max_response_bytes,
        plugins,
    })
}

/// Rejects a field that the route's target kind does not own, and a provider route without a
/// protocol.
///
/// The check is per kind rather than per field list so the error names the field the operator wrote:
/// an upstream route has nothing to convert, so `protocol`, `mode` and `max_response_bytes` would
/// otherwise be read as defaults and silently dropped, and a provider route has nothing to route on
/// without the name of the client protocol.
fn check_target_fields(
    target: &RouteTarget,
    protocol: Option<&str>,
    mode: Option<RouteMode>,
    max_response_bytes: Option<u32>,
) -> Result<(), DocumentError> {
    match target {
        RouteTarget::Upstream(_) => {
            let written = [
                ("routes[].protocol", protocol.is_some()),
                ("routes[].mode", mode.is_some()),
                ("routes[].max_response_bytes", max_response_bytes.is_some()),
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
