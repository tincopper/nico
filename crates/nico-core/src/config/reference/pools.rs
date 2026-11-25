//! Resolves provider and upstream route pools.

use std::collections::HashSet;

use super::{
    super::{
        error::DocumentError,
        raw::{RawPoolMember, RawUpstreamPoolMember},
        route::{Pool, PoolMember, PoolStrategy, UpstreamPool, UpstreamPoolMember},
    },
    ensure_known,
};

/// Inclusive upper bound for pool member weights.
pub(super) const MAX_POOL_WEIGHT: u16 = 1000;

/// Resolves one route's `upstreams` list into a pool.
pub(super) fn resolve_upstream_pool(
    route: &str,
    raw: Vec<RawUpstreamPoolMember>,
    strategy: Option<PoolStrategy>,
    upstreams: &HashSet<&str>,
) -> Result<UpstreamPool, DocumentError> {
    if raw.is_empty() {
        return Err(DocumentError::PoolEmpty {
            id: route.to_owned(),
        });
    }
    let mut seen = HashSet::with_capacity(raw.len());
    let mut members = Vec::with_capacity(raw.len());
    for member in raw {
        ensure_known(upstreams, "upstream", &member.upstream)?;
        if !seen.insert(member.upstream.clone()) {
            return Err(DocumentError::DuplicateId {
                kind: "route-upstream-pool-member",
                id: member.upstream,
            });
        }
        let weight = member.weight.unwrap_or(1);
        if !(1..=MAX_POOL_WEIGHT).contains(&weight) {
            return Err(DocumentError::LimitOutOfRange {
                field: "routes[].upstreams[].weight",
                value: u64::from(weight),
            });
        }
        members.push(UpstreamPoolMember::new(member.upstream, weight));
    }
    Ok(UpstreamPool::new(members, strategy.unwrap_or_default()))
}

/// Resolves one route's `providers` list into a pool.
pub(super) fn resolve_pool(
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
