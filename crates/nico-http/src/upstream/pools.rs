//! Process-level selection state, and the per-generation table a request selects from.
//!
//! Two objects with two lifetimes, and keeping them apart is what makes selection both continuous
//! across a reload and consistent within one generation:
//!
//! - [`UpstreamPools`] is the **inheritance store**. It lives as long as the process, is written only
//!   by publication, and remembers the previous generation's members so a member that kept its
//!   identity keeps what the gateway has observed about it. A reload changes the document; it does not
//!   reset how a machine has been behaving.
//! - [`SelectionTable`] is what one generation serves from. [`UpstreamPools::publish`] builds it, it
//!   travels inside the published value, and it is never mutated — so a request that began before a
//!   publication selects from the members its own document named. A live map read by the request path
//!   would instead let a request pair one generation's route with another's member list, and the two
//!   only agree by accident.
//!
//! A member's identity is the whole quadruple `(upstream name, member name, address, port)`. Only an
//! exact match inherits: carrying a count onto a machine the operator just wrote in would report a
//! failure that machine never had. The upstream name scopes the map, so it is the key rather than a
//! field of [`MemberId`].
//!
//! **Where a strategy's state lives is the strategy's business.** [`UpstreamState`] holds an
//! [`Arc<dyn Picker>`](Picker) and does not know whether that picker keeps a counter, a vector or
//! nothing at all. The pool supplies the facts (members, availability, in-flight counts) through
//! [`Members`], asks for one choice, and the picker decides with whatever state it owns. Adding a
//! strategy therefore touches the picker module and one exhaustive `build`, never this file.
//!
//! `least_conn` is why a member keeps an in-flight count here instead of reading Pingora's pool: that
//! pool is process-wide and its connections belong to no request, so it cannot answer "how many
//! requests are using this member right now". The count is held for as long as the request is, by a
//! [`ConnectionGuard`] whose only decrement is `Drop`.
//!
//! A member is only a candidate while [`MemberState::available`] says so, and that verdict is the
//! ejection machine in [`health`]: consecutive failures, a cooldown, and one half-open probe. The
//! verdict is asked in the one place that builds the candidate list, so "is this machine worth a
//! request" is decided once per attempt and never by a hook that happens to know a member. Being a
//! candidate is not a probe: only the member the request is sent to is admitted, so a request routed
//! elsewhere leaves the probe its cooldown produced for the request that reaches the member.

mod state;

use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Mutex, MutexGuard},
    time::Instant,
};

use nico_core::{Document, Upstream};

use super::{Destination, dns::ExpandedMembers};

mod active;
pub(crate) mod health;
pub(crate) mod member;
mod picker;
pub(crate) mod probe;
mod route_pool;

/// The failure vocabulary a hook records, re-exported so no caller spells a member's failure as text.
pub(crate) use health::{Failure as MemberFailure, Outcome as MemberOutcome};
use member::Member;
pub(crate) use member::{ConnectionGuard, MemberId};
use route_pool::RoutePoolState;
use state::UpstreamState;

/// The inheritance store: the members of the last published document, by upstream name.
///
/// One lock, taken by publication only. The request path never touches it: a request reads the
/// [`SelectionTable`] its generation carries, so the two never contend and a slow publication cannot
/// stall a request.
#[derive(Debug, Default)]
pub(crate) struct UpstreamPools {
    previous: Mutex<HashMap<Arc<str>, Arc<UpstreamState>>>,
    /// The route pools of the last published document, by route id.
    ///
    /// The same lifetime rule as the member history: written only by publication, read to inherit a
    /// route pool's picker state when the new document names the same route again.
    previous_route_pools: Mutex<HashMap<Arc<str>, Arc<RoutePoolState>>>,
}

impl UpstreamPools {
    /// Builds the table one publication serves from, inheriting state by member identity.
    ///
    /// `expanded` is the publication's resolved hostname members (see
    /// [`dns`](super::dns)): a hostname endpoint contributes one member per resolved address, a
    /// literal endpoint keeps its own. Called once per successful publication, on the publishing
    /// thread. It is the only writer of the store, which is why a plain lock is enough and why the
    /// returned table can be immutable.
    pub(crate) fn publish(
        &self,
        document: &Document,
        expanded: &ExpandedMembers,
    ) -> Arc<SelectionTable> {
        let mut previous = self.lock();
        let mut previous_route_pools = self.lock_route_pools();
        let mut history = HashMap::with_capacity(document.upstreams().len());
        let mut table = HashMap::with_capacity(document.upstreams().len());
        for upstream in document.upstreams() {
            let key: Arc<str> = Arc::from(upstream.name());
            let inherited = previous.get(&key).map(Arc::clone);
            let state = Arc::new(UpstreamState::inherit(
                upstream,
                inherited.as_deref(),
                expanded,
            ));
            history.insert(Arc::clone(&key), Arc::clone(&state));
            table.insert(key, state);
        }
        // The route pools: one per route that distributes across upstreams, keyed by the route id the
        // logs already use. Inheritance is the member history's rule seen from the route's side: a
        // route the document still names keeps its picker's state, and one it dropped is forgotten.
        let mut pool_history = HashMap::with_capacity(document.routes().len());
        let mut route_pools = HashMap::with_capacity(document.routes().len());
        for route in document.routes() {
            let Some(pool) = route.upstream_pool() else {
                continue;
            };
            let key: Arc<str> = Arc::from(route.id());
            let inherited = previous_route_pools.get(&key).map(Arc::clone);
            let state = Arc::new(RoutePoolState::inherit(pool, inherited.as_deref()));
            pool_history.insert(Arc::clone(&key), Arc::clone(&state));
            route_pools.insert(key, state);
        }
        // Replacing the map is what keeps inheritance bounded: a member the document no longer names
        // is forgotten here, so a document that renames its members on every reload cannot accumulate
        // state for machines that are gone. A member a request in flight is still using stays alive
        // through that generation's table, which is the only reader that still needs it.
        *previous = history;
        *previous_route_pools = pool_history;
        Arc::new(SelectionTable {
            upstreams: table,
            route_pools,
        })
    }

    /// Takes the store's lock, recovering from a poisoned one.
    ///
    /// Nothing that runs under the lock can panic, so a poisoned guard still describes a consistent
    /// map, and recovering keeps a publication alive instead of turning a panic elsewhere into a
    /// permanently unselectable upstream.
    fn lock(&self) -> MutexGuard<'_, HashMap<Arc<str>, Arc<UpstreamState>>> {
        match self.previous.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Takes the route-pool history's lock, recovering the same way [`Self::lock`] does.
    fn lock_route_pools(&self) -> MutexGuard<'_, HashMap<Arc<str>, Arc<RoutePoolState>>> {
        match self.previous_route_pools.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Returns the identities the store would inherit from, for tests about what a publication
    /// forgets as well as what it keeps.
    #[cfg(test)]
    pub(crate) fn inherited_ids(&self, upstream: &str) -> Option<Vec<MemberId>> {
        self.lock().get(upstream).map(|state| {
            state
                .probe_targets()
                .iter()
                .map(|member| member.state().id().clone())
                .collect()
        })
    }
}

/// The members one published generation may select from.
///
/// Immutable and owned by the published value, so the document a request was routed with and the
/// members it selects from are one generation by construction.
#[derive(Debug)]
pub struct SelectionTable {
    upstreams: HashMap<Arc<str>, Arc<UpstreamState>>,
    /// The route pools this generation published, by route id. A route the document gave no pool is
    /// absent, and its selection goes straight to its single target.
    route_pools: HashMap<Arc<str>, Arc<RoutePoolState>>,
}

impl SelectionTable {
    /// Iterates every upstream this generation published, in map order.
    ///
    /// The probe scheduler walks this; the order is not the document's, and nothing a probe does
    /// depends on it being anything but stable within one pass. Unused until the server's health
    /// service drives the scheduler.
    pub(crate) fn iter_upstreams(&self) -> impl Iterator<Item = &Arc<UpstreamState>> {
        self.upstreams.values()
    }

    /// Returns the state of the upstream named `name`, when this generation published it.
    pub(crate) fn upstream(&self, name: &str) -> Option<&Arc<UpstreamState>> {
        self.upstreams.get(name)
    }

    /// Returns the route pool this generation published for `route`, when the route has one.
    pub(crate) fn route_pool(&self, route: &str) -> Option<&Arc<RoutePoolState>> {
        self.route_pools.get(route)
    }

    /// Picks the upstream one request is sent to, from `pool`'s members.
    ///
    /// The candidates are the pool's members whose upstream still has an available member — the
    /// upstream-level form of the member pool's own candidate list, so a strategy cannot choose an
    /// upstream the gateway has just decided against.
    pub(crate) fn pick_upstream<'a>(
        &self,
        pool: &'a RoutePoolState,
        client: Option<IpAddr>,
        now: Instant,
    ) -> Option<&'a Arc<str>> {
        pool.select(&self.upstreams, client, now)
    }

    /// Returns how many upstreams this generation can select from.
    pub(crate) fn len(&self) -> usize {
        self.upstreams.len()
    }
}

/// One upstream's members and the picker its document named.
///
/// Built by [`UpstreamPools::publish`] from the document's entry and the previous state of the same
/// name; read by the request path and never mutated, except for the counters that belong to the
/// members (in-flight, health) and to the picker.
/// One request's choice: where it connects, and the count that keeps it there until it ends.
#[derive(Debug)]
pub(crate) struct Selected {
    /// The destination to connect to, named by the upstream's logical host.
    pub(crate) destination: Destination,
    /// The in-flight count this request holds, released when it is dropped.
    pub(crate) guard: ConnectionGuard,
}

#[cfg(test)]
mod tests;
