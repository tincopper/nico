//! One member of a published upstream: its identity, its counters, and one request's hold on it.
//!
//! Three values with three lifetimes, and keeping them apart is the whole file:
//!
//! - [`MemberId`] is what survives a publication. It is the quadruple the upstream map keys on
//!   (name, address, port) minus the upstream name, which is the map's key. A generation that names
//!   the same machine keeps the state attached to this identity, and one that names another machine
//!   starts over.
//! - [`MemberState`] is what the gateway has observed: how many requests are using the member right
//!   now, and its failure streak ([`Health`]). It is `Arc`-shared so a request that selected from the
//!   previous generation keeps counting against the machine it is actually using.
//! - [`ConnectionGuard`] is one request's hold on that state. Its only decrement is `Drop`, which is
//!   what makes every exit path — a response, a refusal, a cancellation — release the count exactly
//!   once.
//!
//! The guard is also where a member's health becomes observable, because it is the only value that
//! knows both the member and the upstream it belongs to: one `warn!` when a member leaves rotation,
//! one `info!` when it comes back, and the ejection counter. Those labels come from the document's
//! names and never from a request, so the cardinality is the size of the configuration.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Instant,
};

use nico_core::{Ejection, Endpoint};

use super::health::{Failure, Health, Outcome, Transition};

/// The identity a member keeps across publications.
///
/// Everything a selection or an operator could observe about the member is keyed by it: the name for
/// logs and metrics, the address and port for the connection. Changing any of the three is a different
/// machine as far as the gateway is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemberId {
    name: Arc<str>,
    address: Arc<str>,
    port: u16,
}

impl MemberId {
    /// Reads the identity of one document member.
    pub(super) fn of(endpoint: &Endpoint) -> Self {
        Self {
            name: Arc::from(endpoint.name()),
            address: Arc::from(endpoint.address()),
            port: endpoint.port(),
        }
    }

    /// Returns the member's name, unique within its upstream.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Returns the name as a handle, so a metric label clones one `Arc` instead of copying the bytes.
    pub(crate) fn name_handle(&self) -> &Arc<str> {
        &self.name
    }

    /// Returns the address a request to this member connects to, as a handle it can be shared by.
    pub(crate) fn address(&self) -> &Arc<str> {
        &self.address
    }

    /// Returns the member's port.
    pub(crate) const fn port(&self) -> u16 {
        self.port
    }
}

/// What the gateway has observed about one member, kept for as long as its identity is.
#[derive(Debug)]
pub(crate) struct MemberState {
    id: MemberId,
    active_connections: AtomicU32,
    health: Health,
}

impl MemberState {
    /// Builds the state of a member nothing has been observed about yet.
    pub(super) fn new(id: MemberId) -> Self {
        Self {
            id,
            active_connections: AtomicU32::new(0),
            health: Health::default(),
        }
    }

    /// Returns the identity this state belongs to.
    pub(crate) fn id(&self) -> &MemberId {
        &self.id
    }

    /// Returns how many requests are using this member right now.
    pub(crate) fn active_connections(&self) -> u32 {
        self.active_connections.load(Ordering::Relaxed)
    }

    /// Returns whether this member may be considered at `now`, without claiming its probe.
    ///
    /// The candidate filter of a selection asks this; only the member that is actually chosen is
    /// admitted ([`MemberState::admit`]), so passing a cooled-down member over does not spend the one
    /// probe its cooldown produced.
    pub(crate) fn available(&self, policy: Ejection, now: Instant) -> bool {
        self.health.available(policy, now)
    }

    /// Admits the caller, claiming the single probe when the member is ejected and one is due.
    ///
    /// The single entry point for the health verdict, so no caller can decide on its own that an
    /// ejected member is worth one more try. Asking is not free of consequences by design: a member
    /// whose cooldown expired admits this caller as the one probe, and a second caller racing it is
    /// refused.
    pub(crate) fn admit(&self, policy: Ejection, now: Instant) -> bool {
        self.health.admit(policy, now)
    }

    /// Records what one attempt reported, returning the availability change it caused.
    pub(super) fn record(&self, policy: Ejection, outcome: Outcome, now: Instant) -> Transition {
        self.health.record(policy, outcome, now)
    }

    /// Returns the consecutive failures recorded so far, for the state machine's tests.
    #[cfg(test)]
    pub(crate) fn failures(&self) -> u32 {
        self.health.failures()
    }
}

/// Keeps one member's in-flight count for as long as a request uses it, and reports what it did.
///
/// The only decrement is `Drop` on purpose: a completed response, a refusal, a cancellation and a hook
/// that aborted all release the count the same way, so no failure branch can forget one. The guard
/// lives in [`HttpContext`](crate::HttpContext), which is dropped exactly when the request is.
#[derive(Debug)]
pub(crate) struct ConnectionGuard {
    /// The upstream's name: a metric label and a log field, never a member address.
    upstream: Arc<str>,
    member: Arc<MemberState>,
    /// The document's ejection budget, read once at selection.
    policy: Ejection,
    /// Whether the upstream published more than one member, which decides whether the request record
    /// names the member at all: a single-member deployment keeps exactly the log shape it had.
    multi_member: bool,
}

impl ConnectionGuard {
    /// Counts one more request against `member`.
    pub(super) fn begin(
        upstream: Arc<str>,
        member: Arc<MemberState>,
        policy: Ejection,
        multi_member: bool,
    ) -> Self {
        member.active_connections.fetch_add(1, Ordering::Relaxed);
        Self {
            upstream,
            member,
            policy,
            multi_member,
        }
    }

    /// Returns the member's name when the upstream has more than one member.
    ///
    /// `None` for a single-member upstream: the request record's `upstream` field already names the
    /// only machine, and writing a member name there would change the shape of every existing
    /// deployment's logs to say nothing new.
    pub(crate) fn endpoint(&self) -> Option<&str> {
        self.multi_member.then(|| self.member.id().name())
    }

    /// Reports what the member did with this request, logging and counting a change of availability.
    ///
    /// Called at most once per request and never by the response-body stage: the two events this
    /// emits are facts about the member, and a chunk of a body is not a second answer.
    pub(crate) fn record(&self, outcome: Outcome, now: Instant) {
        let transition = self.member.record(self.policy, outcome, now);
        let endpoint = self.member.id().name();
        let upstream = &self.upstream;
        match transition {
            Transition::None => {}
            Transition::Ejected(reason) => {
                metrics::counter!(
                    "gateway_upstream_ejected_total",
                    "upstream" => metrics::SharedString::from(Arc::clone(&self.upstream)),
                    "endpoint" => metrics::SharedString::from(Arc::clone(
                        self.member.id().name_handle()
                    )),
                )
                .increment(1);
                log_ejection(upstream, endpoint, reason);
            }
            // The probe answered with a failure: the member stays out and its cooldown restarts. It is
            // not a second ejection — nothing came back — so the counter does not move, but an
            // operator who just repaired a machine needs to see that the probe did not take.
            Transition::ProbeFailed(reason) => log_ejection(upstream, endpoint, reason),
            Transition::Recovered => {
                tracing::info!(
                    upstream = %upstream,
                    upstream.endpoint = %endpoint,
                    "upstream member returned"
                );
            }
        }
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.member
            .active_connections
            .fetch_sub(1, Ordering::Relaxed);
    }
}

/// Logs one member that left rotation, with the failure that decided it.
fn log_ejection(upstream: &str, endpoint: &str, reason: Failure) {
    tracing::warn!(
        upstream = %upstream,
        upstream.endpoint = %endpoint,
        reason = reason.as_str(),
        "upstream member ejected"
    );
}
