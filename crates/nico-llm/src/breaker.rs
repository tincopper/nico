//! The circuit breaker: whether a target admits the next attempt, and what its failures taught it.
//!
//! A breaker is the one piece of long-lived state the request path writes: an endpoint that fails
//! repeatedly must stop absorbing attempts, and one that recovered must be probed before it is
//! trusted again. The state machine is per **target identity** ([`TargetIdentity`]) — the provider
//! name, the base URL, the adapter kind and the breaker parameters — so a generation that compiles
//! an equal identity continues the state the previous one left behind, and a re-tuned endpoint
//! starts over: a state that outlived its own thresholds would open or recover under rules the
//! document no longer states.
//!
//! # The machine
//!
//! ```text
//! closed --连续失败达 threshold--> open --cooldown 到期--> half-open (下一个请求成为探测)
//! half-open --探测成功--> closed          half-open --探测失败--> open
//! ```
//!
//! `closed` admits; `open` skips until its cooldown expires, after which the next request becomes
//! the one probe; while that probe is out, every other request skips. Two rules keep the machine
//! honest under real failures:
//!
//! - **A probe that never reports is retried after one cooldown.** An attempt whose outcome is lost
//!   (a client disconnected mid-exchange, a deadline cancelled the request) would otherwise leave
//!   the breaker half-open forever, and a target that recovered would never be probed again. After
//!   one cooldown in half-open, the next request becomes a fresh probe.
//! - **A failure reported while `open` is ignored.** It belongs to an attempt admitted before the
//!   breaker opened; the operator set the cooldown, and late arrivals do not extend it.
//!
//! `failure_threshold` of `0` disables the machine: failures are still counted, but no request is
//! ever skipped because of them.
//!
//! The state lives in a [`BreakerTable`], the one value a process shares across generations; the
//! decision itself is synchronous, so every call takes `now` and no lock is ever held across an
//! `.await`.

use std::{
    collections::BTreeMap,
    sync::{Mutex, MutexGuard},
    time::Duration,
};

use tokio::time::Instant;

use crate::{
    route::{BreakerParams, TargetIdentity},
    schema::CanonicalError,
};

/// The phase a target's breaker is in, as a log reports it.
///
/// This is the observable vocabulary ([`BreakerPhase::as_str`] feeds the circuit log); the state the
/// machine moves through carries the timestamps it needs and stays private.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerPhase {
    /// Admitting every attempt.
    Closed,
    /// Refusing every attempt until its cooldown expires.
    Open,
    /// One probe is out; every other attempt is refused.
    HalfOpen,
}

impl BreakerPhase {
    /// Returns the log spelling of this phase.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Open => "open",
            Self::HalfOpen => "half_open",
        }
    }
}

/// Whether a target's breaker admits the next attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// The attempt may be sent.
    Admit,
    /// The attempt is skipped without contacting the target, because the breaker is in `phase`.
    Skip(BreakerPhase),
}

/// The outcome one attempt produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// The target answered the attempt.
    Success,
    /// The attempt failed against the target.
    Failure,
}

/// One target's breaker: the machine, and the parameters it moves under.
#[derive(Debug)]
pub(crate) struct Breaker {
    params: BreakerParams,
    /// The cooldown as a duration, converted once from the document's milliseconds.
    cooldown: Duration,
    state: BreakerState,
    consecutive_failures: u32,
    /// The canonical error the most recent failure produced, so a request whose targets are all
    /// skipped can still answer with something real (see [`BreakerTable::last_error`]).
    last_error: Option<CanonicalError>,
}

/// The state the machine moves through, carrying what each phase needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BreakerState {
    Closed,
    Open { opened_at: Instant },
    HalfOpen { entered_at: Instant },
}

impl Breaker {
    /// Builds a closed breaker with no failure history.
    pub(crate) fn new(params: BreakerParams) -> Self {
        Self {
            params,
            cooldown: Duration::from_millis(u64::from(params.cooldown_ms())),
            state: BreakerState::Closed,
            consecutive_failures: 0,
            last_error: None,
        }
    }

    /// Returns the phase this breaker is in, in the log's vocabulary.
    pub(crate) const fn phase(&self) -> BreakerPhase {
        match self.state {
            BreakerState::Closed => BreakerPhase::Closed,
            BreakerState::Open { .. } => BreakerPhase::Open,
            BreakerState::HalfOpen { .. } => BreakerPhase::HalfOpen,
        }
    }

    /// Decides whether the next attempt at `now` may contact the target.
    ///
    /// An `open` breaker whose cooldown has expired admits the caller **as the probe**: the state
    /// moves to half-open here, so exactly one request crosses while the probe is out. A half-open
    /// breaker whose probe never reported admits a fresh probe after one cooldown (see the module
    /// documentation).
    pub(crate) fn admit(&mut self, now: Instant) -> Admission {
        match self.state {
            BreakerState::Closed => Admission::Admit,
            BreakerState::Open { opened_at } => {
                if now.duration_since(opened_at) >= self.cooldown {
                    self.state = BreakerState::HalfOpen { entered_at: now };
                    Admission::Admit
                } else {
                    Admission::Skip(BreakerPhase::Open)
                }
            }
            BreakerState::HalfOpen { entered_at } => {
                if now.duration_since(entered_at) >= self.cooldown {
                    self.state = BreakerState::HalfOpen { entered_at: now };
                    Admission::Admit
                } else {
                    Admission::Skip(BreakerPhase::HalfOpen)
                }
            }
        }
    }

    /// Records one attempt's outcome at `now`, with the canonical error a failure produced.
    ///
    /// A failure is reported while the breaker is `open` only by an attempt admitted before it
    /// opened; that outcome says nothing new, so the cooldown is not extended.
    pub(crate) fn record(
        &mut self,
        outcome: AttemptOutcome,
        failure: Option<&CanonicalError>,
        now: Instant,
    ) {
        match self.state {
            BreakerState::Closed => match outcome {
                AttemptOutcome::Success => self.consecutive_failures = 0,
                AttemptOutcome::Failure => {
                    self.remember(failure);
                    self.consecutive_failures += 1;
                    if !self.params.is_disabled()
                        && self.consecutive_failures >= self.params.threshold()
                    {
                        self.state = BreakerState::Open { opened_at: now };
                    }
                }
            },
            BreakerState::HalfOpen { .. } => match outcome {
                // The probe spoke for the target as a whole: one answer ends the experiment.
                AttemptOutcome::Success => {
                    self.state = BreakerState::Closed;
                    self.consecutive_failures = 0;
                }
                AttemptOutcome::Failure => {
                    self.remember(failure);
                    self.state = BreakerState::Open { opened_at: now };
                }
            },
            BreakerState::Open { .. } => {}
        }
    }

    /// Keeps the canonical error a failure produced, for the answer an all-targets-skipped request
    /// needs.
    fn remember(&mut self, failure: Option<&CanonicalError>) {
        if let Some(error) = failure {
            self.last_error = Some(error.clone());
        }
    }
}

/// The process-level breaker state of every compiled target, keyed by identity.
///
/// One table serves every generation: an entry is created the first time its target is asked for,
/// and a publication that removed a target drops its state through [`BreakerTable::retain`]. The
/// table is bounded by construction — its keys are the identities the document compiled — so a
/// caller cannot grow it one request at a time.
pub struct BreakerTable {
    entries: Mutex<BTreeMap<TargetIdentity, Breaker>>,
}

impl BreakerTable {
    /// Builds an empty table: a process starts with no opinion about any target.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(BTreeMap::new()),
        }
    }

    /// Decides whether the target `identity` names admits an attempt at `now`.
    ///
    /// The first question about a target creates its breaker from the identity's own parameters, so
    /// a publication that changed them starts a fresh machine.
    pub fn admit(&self, identity: &TargetIdentity, now: Instant) -> Admission {
        let mut entries = self.lock();
        let breaker = entries
            .entry(identity.clone())
            .or_insert_with(|| Breaker::new(identity.params()));
        let before = breaker.phase();
        let admission = breaker.admit(now);
        self.log_transition(identity, before, breaker.phase());
        admission
    }

    /// Records `outcome` for the target `identity` names, observed at `now`.
    ///
    /// A failure carries the canonical error it produced: the one answer a request whose targets
    /// are all skipped can fall back to, because no response arrived to translate.
    pub fn record(
        &self,
        identity: &TargetIdentity,
        outcome: AttemptOutcome,
        failure: Option<&CanonicalError>,
        now: Instant,
    ) {
        let mut entries = self.lock();
        let breaker = entries
            .entry(identity.clone())
            .or_insert_with(|| Breaker::new(identity.params()));
        let before = breaker.phase();
        breaker.record(outcome, failure, now);
        self.log_transition(identity, before, breaker.phase());
    }

    /// Returns the canonical error the target's most recent failure produced.
    ///
    /// A breaker that is open has recorded at least one failure, so a skipped request always finds
    /// an answer here; `None` means the target has never failed, which is exactly when this is not
    /// asked for.
    pub fn last_error(&self, identity: &TargetIdentity) -> Option<CanonicalError> {
        let entries = self.lock();
        entries
            .get(identity)
            .and_then(|breaker| breaker.last_error.clone())
    }

    /// Drops the state of every target `live` does not name.
    ///
    /// The publication calls this once, after it compiled the generation's chains: a target the
    /// document no longer serves has no future attempts to skip, and its breaker would otherwise
    /// remember a failure history for an endpoint that no longer exists.
    pub fn retain(&self, live: impl Fn(&TargetIdentity) -> bool) {
        self.lock().retain(|identity, _| live(identity));
    }

    /// Locks the table, recovering from a poisoned lock by keeping whatever state survived.
    ///
    /// A panic while a breaker mutates must not take the whole table down: losing the failure
    /// history means the machine starts over from `closed`, which is the safe direction — it costs
    /// extra attempts against a bad target, never a skipped request against a good one.
    fn lock(&self) -> MutexGuard<'_, BTreeMap<TargetIdentity, Breaker>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Logs one phase transition, if this call made one.
    ///
    /// A transition is an operator-visible fact (the target changed what it will admit), and the
    /// identity names the target without its base URL or credential.
    fn log_transition(&self, identity: &TargetIdentity, before: BreakerPhase, after: BreakerPhase) {
        if before != after {
            tracing::warn!(
                llm.target = %identity.provider(),
                llm.circuit = after.as_str(),
                "circuit breaker changed phase"
            );
        }
    }
}

impl Default for BreakerTable {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for BreakerTable {
    /// Reports how many targets hold state and nothing about them: an identity's base URL and a
    /// breaker's history are endpoint facts, and this value is logged.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let entries = self.lock();
        formatter
            .debug_struct("BreakerTable")
            .field("targets", &entries.len())
            .finish()
    }
}

// Compiled-in proof that the one table a process shares can cross `.await` and `tokio::spawn`
// boundaries alongside the snapshot it is published with.
const _: fn() = || {
    const fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<BreakerTable>();
};

#[cfg(test)]
mod tests;
