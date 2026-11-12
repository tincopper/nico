//! The pool exclusion table: which members are out of distribution, and until when.
//!
//! A provider pool retires a member for a *category-chosen* while — ten seconds for a spent rate
//! limit, five minutes for a credential that stopped working — and that retirement must outlive the
//! request that discovered it. A table keyed by the request would retire the member only for the
//! request that already knows, which is the case the pool does not need help with: the next request
//! would draw the same broken member again.
//!
//! # Why this is not the breaker
//!
//! The two answer different questions and the difference is load-bearing (design §6):
//!
//! | | exclusion table | breaker |
//! |---|---|---|
//! | asks | should this member take traffic for a few seconds | can this target serve at all |
//! | opened by | a rate limit or a bad credential | a run of service failures |
//! | cooldown | the category's, fixed | the document's `cooldown_ms` |
//!
//! Merging them would let one `429` open a breaker for the cooldown a real outage earns, so a
//! throttled provider would look like a broken one to every route that names it.
//!
//! # What is keyed
//!
//! A `(route, member)` pair, not a [`TargetIdentity`](crate::TargetIdentity). The same provider may
//! be a pool member on one route and never distributed on another; retiring it for the route that
//! hit the limit is the accurate statement, and retiring it everywhere would turn one route's rate
//! limit into a deployment-wide outage.
//!
//! The state is small and short-lived by construction — one entry per retired member, dropped when
//! its cooldown passes — so it is a plain map behind one lock, and the request path only reads it.

use std::{collections::HashMap, time::Duration};

use tokio::time::Instant;

/// What retired a member, kept so a later reader can say why without re-deriving it.
///
/// The reason is not used for the cooldown (that is stored with the entry) but for the operator: a
/// member that keeps reappearing in this table as `rate_limited` is a quota problem, and one that
/// keeps appearing as `credential` is a key problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The provider answered `429`: its rate limit is spent for now.
    RateLimited,
    /// The provider answered `401` or `403`: the credential is wrong, expired or out of quota.
    Credential,
}

impl Reason {
    /// Returns the `llm.excluded` value this reason is logged and counted under.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RateLimited => "rate_limited",
            Self::Credential => "credential",
        }
    }

    /// Returns how long a member retired for this reason stays out of distribution.
    ///
    /// A property of the *category*, not of the deployment: a rate-limit window is seconds to a
    /// minute, a broken credential does not heal until an operator rotates a key. A deployment that
    /// wants a slower reaction to real outages tunes the provider's `circuit_breaker.cooldown_ms`,
    /// which is the knob for that.
    #[must_use]
    pub const fn cooldown(self) -> Duration {
        match self {
            Self::RateLimited => Duration::from_secs(10),
            Self::Credential => Duration::from_secs(300),
        }
    }
}

/// Which member of which route is retired.
///
/// Both halves are the document's own names, so the key's cardinality is the configuration's — a
/// request cannot grow this map by choosing a header.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Entry {
    route: String,
    provider: String,
}

impl Entry {
    /// Names one pool member of one route.
    #[must_use]
    pub fn new(route: impl Into<String>, provider: impl Into<String>) -> Self {
        Self {
            route: route.into(),
            provider: provider.into(),
        }
    }

    /// Returns the route id this entry belongs to.
    #[must_use]
    pub fn route(&self) -> &str {
        &self.route
    }

    /// Returns the provider name that was retired.
    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }
}

/// The members currently out of distribution, by `(route, member)`.
#[derive(Debug, Default)]
pub struct ExclusionTable {
    entries: std::sync::Mutex<HashMap<Entry, Retired>>,
}

/// One retired member: when it may be offered again, and why it left.
#[derive(Debug, Clone, Copy)]
struct Retired {
    until: Instant,
    reason: Reason,
}

impl ExclusionTable {
    /// Builds an empty table: a process starts with every member in distribution.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reports whether `entry` may take traffic at `now`.
    ///
    /// An expired entry is dropped on the way out, so the table holds only members that are actually
    /// out — which is what keeps it from growing with every rate limit the deployment ever saw.
    pub fn admits(&self, entry: &Entry, now: Instant) -> bool {
        let mut entries = self.lock();
        match entries.get(entry) {
            None => true,
            Some(retired) if retired.until <= now => {
                entries.remove(entry);
                true
            }
            Some(_) => false,
        }
    }

    /// Retires `entry` for `reason`'s cooldown, measured from `now`.
    ///
    /// A second exclusion of the same member restarts the cooldown rather than extending it: the
    /// newer observation is the better one, and the categories have fixed windows so there is nothing
    /// to accumulate. Returns the instant the member may be offered again, so a caller can report it.
    pub fn exclude(&self, entry: Entry, reason: Reason, now: Instant) -> Instant {
        let until = now + reason.cooldown();
        self.lock().insert(entry, Retired { until, reason });
        until
    }

    /// Returns why `entry` is out and until when, for a reader that needs the state rather than the
    /// verdict.
    #[must_use]
    pub fn retired(&self, entry: &Entry) -> Option<(Instant, Reason)> {
        self.lock()
            .get(entry)
            .map(|retired| (retired.until, retired.reason))
    }

    /// Drops every entry whose cooldown has passed at `now`, returning how many were dropped.
    ///
    /// Publication calls this instead of relying on the read path alone: a member the document no
    /// longer names would otherwise stay in the table until its cooldown expired, and an operator
    /// reading the state would see a member that cannot receive traffic at all.
    pub fn sweep(&self, now: Instant) -> usize {
        let mut entries = self.lock();
        let before = entries.len();
        entries.retain(|_, retired| retired.until > now);
        before - entries.len()
    }

    /// Returns how many members are out of distribution, for the publication log.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Returns whether no member is out of distribution.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Takes the table's lock, recovering a poisoned one.
    ///
    /// Nothing inside the lock allocates or awaits beyond a map insert, so a poisoned guard still
    /// describes a consistent table, and recovering keeps one panic elsewhere from turning every
    /// member into one nobody may select.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Entry, Retired>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests;
