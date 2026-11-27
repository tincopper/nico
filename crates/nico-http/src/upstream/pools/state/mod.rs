//! One published upstream's runtime state: its members, its strategy's picker and the facts a
//! selection and the probe scheduler read.
//!
//! Extracted from `pools.rs` when the probe accessors joined: the file holds the store and the
//! selection table, this one holds the per-upstream state they publish.

use std::{fmt, net::IpAddr, sync::Arc, time::Instant};

use nico_core::{Ejection, HealthCheck, Scheme, Strategy};
use nico_lb::{Members as _, Picker, View};

use super::{
    Destination, ExpandedMembers, Selected, Upstream, active,
    member::{ConnectionGuard, Member, MemberId, MemberState, MembersView},
    picker,
};

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
    /// The active check this generation's members are probed with, when the document enabled one.
    /// The active check this generation's members are probed with, when the document enabled one.
    health_check: Option<HealthCheck>,
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
    /// a member whose identity is unchanged and starts fresh otherwise. A hostname endpoint's
    /// expansion contributes one member per resolved address — same name and weight, address is the
    /// machine — so a name that resolves differently between publications inherits per machine, and
    /// an address that stopped resolving leaves the rotation with its member. The picker is carried
    /// across by its own rule ([`Picker::inherit`]), which is the same decision seen from the
    /// strategy's side — but only when the strategy itself is unchanged, because a picker of one kind
    /// cannot inherit the state of another.
    pub(super) fn inherit(
        upstream: &Upstream,
        previous: Option<&Self>,
        expanded: &ExpandedMembers,
    ) -> Self {
        let endpoints = upstream.endpoints();
        let previous_members = previous.map_or(&[][..], |state| state.members.list());
        // One spec per *member*, not per endpoint: the expansion's address list is what decides the
        // member count, and a literal endpoint is its own single member.
        let specs: Vec<(MemberId, u16)> = endpoints
            .iter()
            .flat_map(
                |endpoint| match expanded.addresses(upstream.name(), endpoint.name()) {
                    Some(addresses) => addresses
                        .iter()
                        .map(|address| {
                            (
                                MemberId::new(
                                    Arc::from(endpoint.name()),
                                    Arc::clone(address),
                                    endpoint.port(),
                                ),
                                endpoint.weight(),
                            )
                        })
                        .collect::<Vec<_>>(),
                    None => vec![(MemberId::of(endpoint), endpoint.weight())],
                },
            )
            .collect();
        let health_check = upstream.health_check();
        let fingerprint = health_check.map(active::CheckFingerprint::of);
        let mut members = Vec::with_capacity(specs.len());
        for (id, weight) in &specs {
            let inherited = previous_members
                .iter()
                .position(|member| member.state().id() == id);
            let state = match inherited {
                Some(old) => Arc::clone(&previous_members[old].state),
                None if health_check.is_some() => Arc::new(MemberState::new_probed(id.clone())),
                None => Arc::new(MemberState::new(id.clone())),
            };
            // A member that begins probing this generation records which check its counts belong
            // to; a member whose check changed resets its active state under the new fingerprint.
            if let (Some(active), Some(fp)) = (state.active_health(), fingerprint) {
                // A member whose check parameters changed counts under a different fingerprint:
                // its new checker starts from its own zero rather than reading old counts.
                let reset = active.under().is_some_and(|under| under != fp);
                active.counted_under(fp);
                let _ = reset;
            }
            members.push(Member {
                state,
                weight: *weight,
            });
        }
        let names: Vec<Arc<str>> = specs
            .iter()
            .map(|(id, _weight)| Arc::clone(id.name_handle()))
            .collect();
        let picker = Self::inherit_picker(upstream.strategy(), previous, &names);
        Self {
            name: Arc::from(upstream.name()),
            scheme: upstream.scheme(),
            authority: Arc::from(upstream.host()),
            ejection: upstream.ejection(),
            strategy: upstream.strategy(),
            health_check: upstream.health_check().cloned(),
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

    /// Reports whether any member of this upstream may serve at `now`.
    ///
    /// The route pool's candidate filter asks this: an upstream whose every member is out of rotation
    /// is not a candidate, so a pool strategy cannot choose it and a request cannot land on an
    /// upstream the gateway has just decided against.
    pub(crate) fn has_available(&self, now: Instant) -> bool {
        self.members
            .list()
            .iter()
            .any(|member| member.state().available(self.ejection, now))
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
    /// `tried` is the walk's memory: the members this request already met, excluded from the
    /// candidates so a retry round cannot be sent to the machine that just failed. An empty list is
    /// a request's first round, which selects freely among the available members.
    ///
    /// Being a candidate is not being admitted: only the member this request is actually sent to
    /// claims the probe its cooldown produced, so a request that the strategy routes elsewhere leaves
    /// that probe for the request that reaches the member. A claim lost to a racing request is not a
    /// failure either — the member is now somebody else's probe, so this request takes the remaining
    /// candidates instead of a second probe.
    ///
    /// The guard is created here, so a member's in-flight count starts before the connection and is
    /// released only when the request's context is dropped.
    pub(crate) fn select_excluding(
        &self,
        tried: &[MemberId],
        client: Option<IpAddr>,
        now: Instant,
    ) -> Option<Selected> {
        let mut candidates: Vec<usize> = (0..self.members.len())
            .filter(|&index| {
                self.members.list()[index]
                    .state()
                    .available(self.ejection, now)
                    && !tried
                        .iter()
                        .any(|id| id == self.members.list()[index].state().id())
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

    /// Returns the members this generation published, in document order.
    pub(crate) fn probe_targets(&self) -> &[Member] {
        self.members.list()
    }

    /// Returns the upstream's document name, for the probe metric labels.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Returns the name as a handle, so the metric label clones one `Arc` instead of copying.
    pub(crate) fn name_handle(&self) -> &Arc<str> {
        &self.name
    }

    /// Returns the upstream's scheme: whether a probe connects over TLS.
    pub(crate) fn scheme(&self) -> Scheme {
        self.scheme
    }

    /// Returns the logical host a probe's Host header and TLS SNI carry.
    pub(crate) fn authority(&self) -> &str {
        &self.authority
    }

    /// Returns this generation's active health check, when one is enabled.
    pub(crate) fn health_check(&self) -> Option<&HealthCheck> {
        self.health_check.as_ref()
    }
}
