//! One route's upstream pool: which upstream serves, and the picker its strategy built.
//!
//! The pool chooses between services; the chosen upstream's member pool chooses between machines,
//! and its rules live in [`super::pools`]. What belongs here is the route-level state alone: the
//! member names it draws among, the weights it reads, and the picker whose state is inherited by
//! route identity across publications.

use std::{collections::HashMap, fmt, net::IpAddr, sync::Arc, time::Instant};

use nico_core::{PoolStrategy, UpstreamPool};
use nico_lb::{Members, Picker, View};

use super::{UpstreamState, picker};

/// One route's upstream pool, and the picker its strategy built.
///
/// The pool chooses *which upstream* serves; the chosen upstream's own member pool chooses *which
/// machine*. The state here is the strategy's alone — a rotation position, a rendezvous rule — and it
/// is inherited by identity (the route id) the same way a member's counters are: a reload that names
/// the same route keeps the rotation, and one that drops the route forgets it.
pub(crate) struct RoutePoolState {
    strategy: PoolStrategy,
    /// The upstream names in document order — the member set the strategy reads.
    members: Vec<Arc<str>>,
    weights: Vec<u16>,
    picker: Arc<dyn Picker>,
}

impl fmt::Debug for RoutePoolState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RoutePoolState")
            .field("strategy", &self.strategy)
            .field("members", &self.members)
            .field("weights", &self.weights)
            .field("picker", &self.picker)
            .finish()
    }
}

impl RoutePoolState {
    /// Builds the pool one publication serves from, inheriting the previous picker where it may.
    ///
    /// A strategy the document kept keeps its picker's state — the rotation position of a
    /// `round_robin` pool does not restart because a weight changed; a strategy the document changed
    /// starts fresh, because one picker's state means nothing to another's.
    pub(super) fn inherit(pool: &UpstreamPool, previous: Option<&Self>) -> Self {
        let strategy = pool.strategy();
        let members: Vec<Arc<str>> = pool
            .members()
            .iter()
            .map(|member| Arc::from(member.upstream()))
            .collect();
        let weights: Vec<u16> = pool
            .members()
            .iter()
            .map(|member| member.weight())
            .collect();
        let picker = match previous {
            Some(previous) if previous.strategy == strategy => previous
                .picker
                .inherit(previous.members.as_slice(), &members),
            _ => picker::build_route_pool_picker(strategy),
        };
        Self {
            strategy,
            members,
            weights,
            picker,
        }
    }

    /// Picks the upstream one request is sent to, among the candidates that can serve it.
    ///
    /// A candidate is a member whose upstream still has an available member — the availability
    /// verdict stays with the upstream's own machine, and this filter only asks it once per request.
    /// `None` when no member's upstream can serve: the caller answers with the refusal an empty
    /// member pool would have produced.
    pub(super) fn select(
        &self,
        upstreams: &HashMap<Arc<str>, Arc<UpstreamState>>,
        client: Option<IpAddr>,
        now: Instant,
    ) -> Option<&Arc<str>> {
        let candidates: Vec<usize> = (0..self.members.len())
            .filter(|&index| {
                upstreams
                    .get(&self.members[index])
                    .is_some_and(|state| state.has_available(now))
            })
            .collect();
        let members = self.view();
        let view = View::new(&members, &candidates, client);
        let index = self.picker.pick(&view)?;
        self.members.get(index)
    }

    /// The member facts a strategy reads: an upstream name, its weight, and no connection count —
    /// the pool chooses between services, and no connection is held at this level.
    fn view(&self) -> impl Members + '_ {
        struct PoolView<'a> {
            members: &'a [Arc<str>],
            weights: &'a [u16],
        }
        impl Members for PoolView<'_> {
            fn len(&self) -> usize {
                self.members.len()
            }

            fn weight(&self, index: usize) -> u16 {
                self.weights.get(index).copied().unwrap_or_default()
            }

            fn active(&self, _index: usize) -> u32 {
                0
            }

            fn name(&self, index: usize) -> &str {
                self.members.get(index).map_or("", |name| name)
            }
        }
        PoolView {
            members: &self.members,
            weights: &self.weights,
        }
    }
}
