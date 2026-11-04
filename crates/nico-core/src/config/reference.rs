//! Reference validation: does a route name something that exists, exactly once?
//!
//! Two decisions belong here, and only these two:
//!
//! 1. **Target shape** — every route names exactly one upstream or provider. The two mistakes get
//!    their own error ([`DocumentError::RouteTargetMissing`] /
//!    [`DocumentError::RouteTargetAmbiguous`]) instead of surfacing as a generic schema failure,
//!    and the resolved name becomes a [`RouteTarget`] so the illegal states are unrepresentable
//!    afterwards.
//! 2. **Existence and uniqueness** — route ids are globally unique, upstream and provider names are
//!    unique within their collection, and every route target resolves.
//!
//! Field shapes and value ranges are [`super::semantic`]'s job. Keeping the two apart is what lets
//! a caller tell "the document points at nothing" from "the document is malformed", which is the
//! same split the validation stages expose.

use std::collections::HashSet;

use super::{
    error::DocumentError,
    provider::Provider,
    raw::RawRoute,
    route::{Route, RouteTarget},
    upstream::Upstream,
};

/// Resolves every route target, rejecting dangling and duplicate references.
///
/// The raw routes are taken by value: a target name moves into the returned [`Route`] instead of
/// being cloned, so the checked value and the published value share one allocation.
///
/// Checks run in this order, and the first violation stops validation: repeated route ids, repeated
/// upstream names, repeated provider names, then per route — in document order — the target shape
/// followed by target resolution.
///
/// # Errors
/// - [`DocumentError::DuplicateId`] for a repeated route id, upstream name or provider name.
/// - [`DocumentError::RouteTargetMissing`] when a route names no target.
/// - [`DocumentError::RouteTargetAmbiguous`] when a route names both kinds of target.
/// - [`DocumentError::ReferenceUnknown`] when a target name is not defined.
pub(super) fn validate(
    raw_routes: Vec<RawRoute>,
    upstreams: &[Upstream],
    providers: &[Provider],
) -> Result<Vec<Route>, DocumentError> {
    reject_duplicates("route", raw_routes.iter().map(|route| route.id.as_str()))?;
    reject_duplicates("upstream", upstreams.iter().map(Upstream::name))?;
    reject_duplicates("provider", providers.iter().map(Provider::name))?;

    let known_upstreams: HashSet<&str> = upstreams.iter().map(Upstream::name).collect();
    let known_providers: HashSet<&str> = providers.iter().map(Provider::name).collect();

    raw_routes
        .into_iter()
        .map(|raw| resolve(raw, &known_upstreams, &known_providers))
        .collect()
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

/// Turns a route's two optional target names into exactly one [`RouteTarget`] and checks that the
/// name is defined.
fn resolve(
    raw: RawRoute,
    upstreams: &HashSet<&str>,
    providers: &HashSet<&str>,
) -> Result<Route, DocumentError> {
    let RawRoute {
        id,
        host,
        path_prefix,
        upstream,
        provider,
        timeout_ms,
        max_body_bytes,
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

    Ok(Route {
        id,
        host,
        path_prefix,
        target,
        timeout_ms,
        max_body_bytes,
    })
}

/// Rejects a target name that the collection does not define.
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
