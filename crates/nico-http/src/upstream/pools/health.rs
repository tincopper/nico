//! One member's availability: a streak of failures, a cooldown, and exactly one probe.
//!
//! Ejection is passive — the gateway never probes a member on a timer, it only learns from the
//! requests it was already going to send. That decides the whole shape of this module:
//!
//! ```text
//! closed --max_fails consecutive failures--> ejected --fail_timeout_ms 到期--> one probe
//! probe answered --> closed                 probe failed --> ejected, cooling restarts
//! ```
//!
//! Three rules make the machine honest, and each exists because the obvious alternative is wrong:
//!
//! - **Only one request is the probe.** The cooldown does not put a member back in rotation; it
//!   admits a single request, and every other request keeps skipping the member until that one
//!   reports. A recovered member therefore rejoins on evidence instead of on a timer — and a member
//!   that is still broken absorbs one request per cooldown instead of all of them. The claim is made
//!   by the request that is actually sent ([`Health::admit`]), never by the candidate filter: a
//!   member a strategy considered and passed over was not probed, so its probe must still be there
//!   for the request that does reach it. [`Health::available`] is the question with no consequences.
//! - **A probe that never reports is retried after one cooldown.** A request can end without
//!   reporting: the client disconnected, the gateway began shutting down, the request's own budget
//!   expired. Without this rule such a request would hold the probe forever and a member that
//!   recovered would never be asked again. One probe per cooldown is the bound.
//! - **A failure reported for an attempt admitted before the ejection is ignored.** It says nothing
//!   new about the member, and letting late failures extend the cooldown would let one bad burst keep
//!   a machine out for as long as the burst lasts. A late *answer* is treated the other way: it is
//!   evidence the machine is serving, so it returns the member even though it was not the probe. The
//!   cooldown bounds how long the gateway waits for evidence, not how it weighs evidence that
//!   arrives on its own.
//!
//! The lock is the machine's read-modify-write: claiming the probe is "read the phase, write the
//! claim", which two atomics cannot express without a window in which two requests both believe they
//! are the probe. Nothing inside the lock allocates, waits or touches the network.

use std::{
    sync::{Mutex, MutexGuard},
    time::{Duration, Instant},
};

use nico_core::Ejection;

/// Why one attempt failed against the member that served it.
///
/// The set is the operator's vocabulary as much as the machine's: each variant is one `reason` value
/// in the ejection log, so a dashboard and a log line name a failure the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Failure {
    /// The connection could not be opened: a refused socket, an unresolvable name, a failed handshake.
    Connect,
    /// The connection spent its budget before it opened — including a TLS handshake that ran out of
    /// the same connect deadline.
    Timeout,
    /// The member answered, and the answer was a 5xx.
    Status,
}

impl Failure {
    /// Returns the `reason` value the ejection log carries.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Timeout => "timeout",
            Self::Status => "5xx",
        }
    }
}

/// What one attempt reported about the member that served it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The member answered the request with anything but a 5xx, including a 4xx.
    ///
    /// A 4xx is the caller's fault and must never eject a member — one bad client could empty a
    /// whole fleet — but it is still an answer: the link is alive, so the streak ends.
    Answered,
    /// The attempt failed against the member.
    Failed(Failure),
}

/// What recording one outcome changed about availability.
///
/// Returned so the caller can log and count the two operator-visible events without reaching into
/// the machine, and so "nothing changed" stays distinguishable from "the member came back". The two
/// ejection arms carry the failure that decided them, because that is the `reason` an operator reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transition {
    /// The member admits the same requests as before this outcome.
    None,
    /// The member left rotation on this outcome.
    Ejected(Failure),
    /// The probe failed: the member stays out and its cooldown restarts.
    ProbeFailed(Failure),
    /// The probe was answered: the member is back in rotation.
    Recovered,
}

/// One member's failure streak and the phase it put the member in.
///
/// `Default` is a member nothing has been observed about: closed, no failures, no probe.
#[derive(Debug, Default)]
pub(crate) struct Health {
    condition: Mutex<Condition>,
}

impl Health {
    /// Reports whether the member would admit a request at `now`, without claiming anything.
    ///
    /// The candidate filter of a selection asks this: a cooled-down member is worth considering, but
    /// considering it is not probing it. A strategy may pass it over, and a probe that was never sent
    /// must still be available for the request that does reach the member — otherwise the member
    /// would be locked out for another whole cooldown every time the rotation preferred someone else.
    pub(crate) fn available(&self, policy: Ejection, now: Instant) -> bool {
        self.lock().admits(cooldown(policy), now)
    }

    /// Admits the caller, claiming the probe when the member is ejected and one is due.
    ///
    /// `&self` with interior mutability because the answer is not a question: a member whose cooldown
    /// expired admits the caller *as* the probe, and the claim has to be made where it is decided.
    /// Two requests racing here is the case the lock exists for — without it both would probe a
    /// member that may still be broken. Called once per selection, for the member about to be used.
    pub(crate) fn admit(&self, policy: Ejection, now: Instant) -> bool {
        let cooldown = cooldown(policy);
        let mut condition = self.lock();
        if !condition.admits(cooldown, now) {
            return false;
        }
        if condition.ejected_at.is_some() {
            condition.probe(now);
        }
        true
    }

    /// Records what one attempt reported, returning the availability change it caused.
    pub(crate) fn record(&self, policy: Ejection, outcome: Outcome, now: Instant) -> Transition {
        let mut condition = self.lock();
        match outcome {
            Outcome::Answered => {
                let was_ejected = condition.ejected_at.is_some();
                condition.close();
                if was_ejected {
                    Transition::Recovered
                } else {
                    Transition::None
                }
            }
            Outcome::Failed(reason) => {
                if condition.ejected_at.is_some() {
                    // Either the probe reporting, or an attempt admitted before the ejection. Only
                    // the probe can say anything about the member now; a late failure would extend
                    // a cooldown it knows nothing about.
                    if condition.probing_since.take().is_some() {
                        condition.ejected_at = Some(now);
                        Transition::ProbeFailed(reason)
                    } else {
                        Transition::None
                    }
                } else {
                    condition.failures = condition.failures.saturating_add(1);
                    // `max_fails` of zero is the document's spelling for "never eject": the streak is
                    // still counted, and no request is ever skipped because of it.
                    if policy.max_fails() != 0 && condition.failures >= policy.max_fails() {
                        condition.ejected_at = Some(now);
                        Transition::Ejected(reason)
                    } else {
                        Transition::None
                    }
                }
            }
        }
    }

    /// Returns the consecutive failures recorded so far, for tests and for a reader that needs the
    /// count rather than the verdict.
    #[cfg(test)]
    pub(crate) fn failures(&self) -> u32 {
        self.lock().failures
    }

    /// Locks the condition, recovering a poisoned one.
    ///
    /// Nothing inside the lock can panic, so a poisoned guard still describes a consistent machine,
    /// and recovering keeps one panic elsewhere from turning a member into one nobody may ever
    /// select again.
    fn lock(&self) -> MutexGuard<'_, Condition> {
        match self.condition.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// The machine's state: what it has counted, and where the member is in the cycle.
#[derive(Debug, Default)]
struct Condition {
    /// Consecutive failures since the last answer. Reset by any answer, including a 4xx.
    failures: u32,
    /// `None` while the member admits. `Some(since)` from the ejection until a probe is answered.
    ejected_at: Option<Instant>,
    /// When the request currently probing the member was admitted.
    ///
    /// `Some` together with `ejected_at` only: while a probe is out the member is still ejected, and
    /// every other request must keep skipping it.
    probing_since: Option<Instant>,
}

impl Condition {
    /// Reports whether the member admits at `now`, leaving the probe unclaimed.
    ///
    /// A member that is not ejected admits unconditionally. An ejected one admits only through its
    /// single probe: while the cooldown runs, and while that probe is out, it admits nobody.
    fn admits(&self, cooldown: Duration, now: Instant) -> bool {
        if self.ejected_at.is_none() {
            return true;
        }
        match self.probing_since {
            // The cooldown expired and nobody is probing: the next request here becomes the probe.
            None => self.cooled_down(cooldown, now),
            // A probe is out. It may have ended without reporting, which would otherwise leave the
            // member unavailable for the life of the process, so a fresh probe is due after one more
            // cooldown.
            Some(claimed) => now.saturating_duration_since(claimed) >= cooldown,
        }
    }

    /// Reports whether the cooldown that started at the ejection has elapsed.
    fn cooled_down(&self, cooldown: Duration, now: Instant) -> bool {
        self.ejected_at
            .is_some_and(|ejected| now.saturating_duration_since(ejected) >= cooldown)
    }

    /// Claims the probe.
    fn probe(&mut self, now: Instant) {
        self.probing_since = Some(now);
    }

    /// Returns the member to rotation at once.
    ///
    /// The streak is cleared together with the phase: a member that just answered has no failures to
    /// remember, and leaving a stale count would eject it again on its next single failure.
    fn close(&mut self) {
        self.failures = 0;
        self.ejected_at = None;
        self.probing_since = None;
    }
}

/// Returns how long an ejected member waits before one probe, as a duration.
///
/// Converted once per decision rather than stored, because the document's milliseconds are the
/// definition and a second copy of the value could drift from the one validation checked.
fn cooldown(policy: Ejection) -> Duration {
    Duration::from_millis(u64::from(policy.fail_timeout_ms()))
}
