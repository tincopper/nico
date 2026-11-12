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

use std::{
    collections::HashMap,
    fmt,
    net::IpAddr,
    sync::{Arc, Mutex, MutexGuard},
    time::Instant,
};

use nico_core::{Document, Ejection, Scheme, Strategy, Upstream};
use nico_lb::{Members, Picker, View};

use super::Destination;

pub(crate) mod health;
pub(crate) mod member;
mod picker;

/// The failure vocabulary a hook records, re-exported so no caller spells a member's failure as text.
pub(crate) use health::{Failure as MemberFailure, Outcome as MemberOutcome};
pub(crate) use member::{ConnectionGuard, MemberId, MemberState};

/// The inheritance store: the members of the last published document, by upstream name.
///
/// One lock, taken by publication only. The request path never touches it: a request reads the
/// [`SelectionTable`] its generation carries, so the two never contend and a slow publication cannot
/// stall a request.
#[derive(Debug, Default)]
pub(crate) struct UpstreamPools {
    previous: Mutex<HashMap<Arc<str>, Arc<UpstreamState>>>,
}

impl UpstreamPools {
    /// Builds the table one publication serves from, inheriting state by member identity.
    ///
    /// Called once per successful publication, on the publishing thread. It is the only writer of the
    /// store, which is why a plain lock is enough and why the returned table can be immutable.
    pub(crate) fn publish(&self, document: &Document) -> Arc<SelectionTable> {
        let mut previous = self.lock();
        let mut history = HashMap::with_capacity(document.upstreams().len());
        let mut table = HashMap::with_capacity(document.upstreams().len());
        for upstream in document.upstreams() {
            let key: Arc<str> = Arc::from(upstream.name());
            let inherited = previous.get(&key).map(Arc::clone);
            let state = Arc::new(UpstreamState::inherit(upstream, inherited.as_deref()));
            history.insert(Arc::clone(&key), Arc::clone(&state));
            table.insert(key, state);
        }
        // Replacing the map is what keeps inheritance bounded: a member the document no longer names
        // is forgotten here, so a document that renames its members on every reload cannot accumulate
        // state for machines that are gone. A member a request in flight is still using stays alive
        // through that generation's table, which is the only reader that still needs it.
        *previous = history;
        Arc::new(SelectionTable { upstreams: table })
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

    /// Returns the identities the store would inherit from, for tests about what a publication
    /// forgets as well as what it keeps.
    #[cfg(test)]
    pub(crate) fn inherited_ids(&self, upstream: &str) -> Option<Vec<MemberId>> {
        self.lock().get(upstream).map(|state| {
            state
                .members
                .list()
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
pub(crate) struct SelectionTable {
    upstreams: HashMap<Arc<str>, Arc<UpstreamState>>,
}

impl SelectionTable {
    /// Returns the state of the upstream named `name`, when this generation published it.
    pub(crate) fn upstream(&self, name: &str) -> Option<&Arc<UpstreamState>> {
        self.upstreams.get(name)
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
pub(crate) struct UpstreamState {
    /// The upstream's document name: the `upstream` label of both metrics and of the request record.
    name: Arc<str>,
    scheme: Scheme,
    /// The logical host: what `Host` and the TLS server name say, never a member's address.
    authority: Arc<str>,
    /// The failure budget every member is judged by, read from the document at publication.
    ejection: Ejection,
    /// The strategy the document named.
    ///
    /// Kept only to decide inheritance: a picker of one kind cannot take over another's state, so
    /// publication compares this before asking [`Picker::inherit`]. It is document data, like
    /// `ejection`, not a description of what the picker keeps.
    strategy: Strategy,
    /// The members in document order, and the facts a picker reads about them.
    members: MembersView,
    /// The strategy's implementation, with whatever state it keeps.
    picker: Arc<dyn Picker>,
}

impl fmt::Debug for UpstreamState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UpstreamState")
            .field("name", &self.name)
            .field("scheme", &self.scheme)
            .field("authority", &self.authority)
            .field("ejection", &self.ejection)
            .field("picker", &self.picker)
            .field("members", &self.members.list())
            .finish()
    }
}

impl UpstreamState {
    /// Builds the state one publication serves from, inheriting the previous state where it may.
    ///
    /// Inheritance is decided per member, on the whole identity: the shared [`MemberState`] comes with
    /// a member whose identity is unchanged and starts fresh otherwise. The picker is carried across
    /// by its own rule ([`Picker::inherit`]), which is the same decision seen from the strategy's side
    /// — but only when the strategy itself is unchanged, because a picker of one kind cannot inherit
    /// the state of another.
    fn inherit(upstream: &Upstream, previous: Option<&Self>) -> Self {
        let endpoints = upstream.endpoints();
        let previous_members = previous.map_or(&[][..], |state| state.members.list());
        let mut members = Vec::with_capacity(endpoints.len());
        for endpoint in endpoints {
            let id = MemberId::of(endpoint);
            let inherited = previous_members
                .iter()
                .position(|member| member.state().id() == &id);
            let state = match inherited {
                Some(old) => Arc::clone(&previous_members[old].state),
                None => Arc::new(MemberState::new(id)),
            };
            members.push(Member {
                state,
                weight: endpoint.weight(),
            });
        }
        let names: Vec<Arc<str>> = endpoints
            .iter()
            .map(|endpoint| Arc::from(endpoint.name()))
            .collect();
        let picker = Self::inherit_picker(upstream.strategy(), previous, &names);
        Self {
            name: Arc::from(upstream.name()),
            scheme: upstream.scheme(),
            authority: Arc::from(upstream.host()),
            ejection: upstream.ejection(),
            strategy: upstream.strategy(),
            members: MembersView { members, names },
            picker,
        }
    }

    /// Returns the picker this generation selects with, carrying the previous one's state when the
    /// strategy is unchanged.
    fn inherit_picker(
        strategy: Strategy,
        previous: Option<&Self>,
        current: &[Arc<str>],
    ) -> Arc<dyn Picker> {
        match previous {
            Some(previous) if previous.strategy == strategy => {
                previous.picker.inherit(previous.members.names(), current)
            }
            // A strategy the document did not have before has no state to inherit, and one picker
            // cannot take over another's: a rotation position means nothing to a smooth rotation.
            _ => picker::build(strategy),
        }
    }

    /// Returns one member for this request, or `None` when none may be contacted.
    ///
    /// `now` is the instant the availability verdict is asked at: a member whose cooldown expired is
    /// only reachable through the clock, and a test that could not pass one would have to sleep to
    /// observe the difference between `open` and `half-open`.
    ///
    /// The candidate list is the whole health decision: a member is asked once, in one place, whether
    /// it may serve — so an ejected member cannot be chosen by a strategy that happens to prefer it.
    /// `None` means every member is out of rotation, which the caller answers as
    /// `502 upstream_unreachable` rather than by retrying a member it just decided against.
    ///
    /// Being a candidate is not being admitted: only the member this request is actually sent to
    /// claims the probe its cooldown produced, so a request that the strategy routes elsewhere leaves
    /// that probe for the request that reaches the member. A claim lost to a racing request is not a
    /// failure either — the member is now somebody else's probe, so this request takes the remaining
    /// candidates instead of a second probe.
    ///
    /// The guard is created here, so a member's in-flight count starts before the connection and is
    /// released only when the request's context is dropped.
    pub(crate) fn select(&self, client: Option<IpAddr>, now: Instant) -> Option<Selected> {
        let mut candidates: Vec<usize> = (0..self.members.len())
            .filter(|&index| {
                self.members.list()[index]
                    .state()
                    .available(self.ejection, now)
            })
            .collect();
        loop {
            let view = View::new(&self.members, &candidates, client);
            let index = self.picker.pick(&view)?;
            if self.members.list()[index].state().admit(self.ejection, now) {
                return Some(self.selected(index));
            }
            candidates.retain(|&candidate| candidate != index);
            if candidates.is_empty() {
                return None;
            }
        }
    }

    /// Builds the value one admitted member produces: where to connect, and the count it holds.
    fn selected(&self, index: usize) -> Selected {
        let member = &self.members.list()[index];
        let id = member.state().id();
        // The one emission point of the selection counter: the label is the document's own name, so
        // the cardinality is the configuration's, not the traffic's.
        metrics::counter!(
            "gateway_upstream_selected_total",
            "upstream" => metrics::SharedString::from(Arc::clone(&self.name)),
            "endpoint" => metrics::SharedString::from(Arc::clone(id.name_handle())),
        )
        .increment(1);
        Selected {
            destination: Destination::new(self.scheme, Arc::clone(id.address()), id.port())
                .with_authority(Arc::clone(&self.authority)),
            guard: ConnectionGuard::begin(
                Arc::clone(&self.name),
                Arc::clone(member.state()),
                self.ejection,
                self.members.len() > 1,
            ),
        }
    }

    /// Returns the members this generation published, for tests that assert what was inherited.
    #[cfg(test)]
    pub(crate) fn members(&self) -> &[Member] {
        self.members.list()
    }

    /// Returns the failure budget this generation published, for the state machine's tests.
    #[cfg(test)]
    pub(crate) fn ejection(&self) -> Ejection {
        self.ejection
    }
}

/// One member of a published upstream: this generation's weight, and the state that outlives it.
///
/// The weight lives here rather than in [`MemberState`] because it is the document's fact: a weight
/// that changed must take effect on the next publication, while the counters it was inherited with
/// must not be recreated just because the number changed.
#[derive(Debug)]
pub(crate) struct Member {
    state: Arc<MemberState>,
    weight: u16,
}

impl Member {
    /// Returns the state this member inherited or started with.
    pub(crate) fn state(&self) -> &Arc<MemberState> {
        &self.state
    }

    /// Returns this generation's weight for the member.
    pub(crate) fn weight(&self) -> u16 {
        self.weight
    }
}

/// One generation's members, read through the accessors a picker may use.
///
/// The names are hoisted into their own vector so a picker receives `&str` without the pool having to
/// hand out its whole member table: a rule may know a member's name and weight, and nothing about how
/// the gateway reaches it.
#[derive(Debug)]
struct MembersView {
    members: Vec<Member>,
    names: Vec<Arc<str>>,
}

impl MembersView {
    /// Returns the members in document order.
    fn list(&self) -> &[Member] {
        &self.members
    }

    /// Returns every member's name, indexed like [`MembersView::list`].
    fn names(&self) -> &[Arc<str>] {
        &self.names
    }
}

impl Members for MembersView {
    fn len(&self) -> usize {
        self.list().len()
    }

    fn weight(&self, index: usize) -> u16 {
        self.list().get(index).map_or(0, Member::weight)
    }

    fn active(&self, index: usize) -> u32 {
        self.list()
            .get(index)
            .map_or(0, |member| member.state().active_connections())
    }

    fn name(&self, index: usize) -> &str {
        self.names.get(index).map_or("", |name| name)
    }
}

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
