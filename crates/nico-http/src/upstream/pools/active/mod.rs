//! One member's *active* health: what its configured probes have reported, counted on its own.
//!
//! This is the second machine a member carries, deliberately separate from the passive
//! [`Health`](super::health::Health):
//!
//! - the passive machine learns from the requests the gateway was already going to send; the
//!   active one learns from the probes the document scheduled. A probe is not a request — it
//!   carries no credential, counts in no quota, and its failures are the check's verdict, not a
//!   caller's experience — so the two counters never mix.
//! - the active machine's sequence is **fall and rise**: `unhealthy_threshold` consecutive failed
//!   probes take the member out, `healthy_threshold` consecutive passed probes return it. Either
//!   outcome clears the other's count, so an alternating probe can never accumulate one.
//! - a member that starts (or inherits a changed check config) starts **Healthy**: the deployment
//!   asked to be told what the probes say, and the first failed probe speaks at its own threshold.
//!   This is the non-mandatory reading every mainstream checker takes; an operator who wants a
//!   member blocked until its first probe lands has that as a separate, explicit knob later.
//!
//! One probe at a time: `begin_probe` claims the member's single slot and `finish_probe` releases
//! it. A slot that is out means a probe is between its deadline and its report — whether that
//! probe belongs to this generation's schedule or to an old one finishing against the same
//! machine is invisible here, and invisible is correct: the member is one machine, and one probe
//! at a time is the bound.
//!
//! The lock guards a `Copy` state; nothing inside it allocates, waits or touches the network, the
//! same rule the passive machine keeps.

use std::{
    sync::{Mutex, MutexGuard},
    time::{Duration, Instant},
};

/// The verdict one probe reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeOutcome {
    /// The member answered within its budget and the status reading passed.
    Passed,
    /// The probe failed: the member did not answer, or answered outside the check's reading.
    Failed,
}

/// What one finished probe changed about the member's active availability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActiveTransition {
    /// The active verdict is unchanged (the counters moved inside their thresholds).
    None,
    /// The member left rotation on this probe: the fall threshold was reached.
    Fell,
    /// The member returned on this probe: the rise threshold was reached.
    Rose,
}

/// Where the member stands in the active checker's judgment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    /// The member takes traffic (subject to the passive machine's own verdict).
    Healthy,
    /// The member is out of rotation until `healthy_threshold` probes pass in a row.
    Unhealthy,
}

impl Default for Status {
    /// The initial verdict is Healthy: the deployment asked to be told what probes say, and the
    /// first failed one speaks at its own threshold.
    fn default() -> Self {
        Self::Healthy
    }
}

/// The active state of one member: status, the two consecutive counts, and the single slot.
#[derive(Debug, Default)]
pub(crate) struct ActiveHealth {
    condition: Mutex<Condition>,
}

impl ActiveHealth {
    /// Builds a fresh state: healthy, no counts, nothing in flight.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Claims the member's single probe slot, resetting both counters when `reset` is asked.
    ///
    /// `reset` is what a config change requests: a check whose parameters changed is a check the
    /// old counts were not counted under, so the new checker starts from its own zero.
    pub(crate) fn begin_probe(&self, reset: bool) -> bool {
        let mut condition = self.lock();
        if condition.in_flight {
            return false;
        }
        condition.in_flight = true;
        condition.last_probe_at = Some(std::time::Instant::now());
        if reset {
            condition.status = Status::Healthy;
            condition.consecutive_failures = 0;
            condition.consecutive_successes = 0;
        }
        true
    }

    /// Reports one probe's verdict, releasing the slot.
    ///
    /// The thresholds are the *check's* — passed in per probe, because they travel with the
    /// document and a reload retunes them without touching what the counts already recorded.
    pub(crate) fn finish_probe(
        &self,
        outcome: ProbeOutcome,
        unhealthy_threshold: u8,
        healthy_threshold: u8,
    ) -> ActiveTransition {
        let mut condition = self.lock();
        condition.in_flight = false;
        match outcome {
            ProbeOutcome::Passed => {
                condition.consecutive_failures = 0;
                condition.consecutive_successes = condition.consecutive_successes.saturating_add(1);
                if condition.status == Status::Unhealthy
                    && condition.consecutive_successes >= healthy_threshold
                {
                    condition.status = Status::Healthy;
                    condition.consecutive_successes = 0;
                    return ActiveTransition::Rose;
                }
                ActiveTransition::None
            }
            ProbeOutcome::Failed => {
                condition.consecutive_successes = 0;
                condition.consecutive_failures = condition.consecutive_failures.saturating_add(1);
                if condition.status == Status::Healthy
                    && condition.consecutive_failures >= unhealthy_threshold
                {
                    condition.status = Status::Unhealthy;
                    condition.consecutive_failures = 0;
                    return ActiveTransition::Fell;
                }
                ActiveTransition::None
            }
        }
    }

    /// Reports whether a probe of this member is due at `now`.
    ///
    /// Due on the member's first probe, or when `interval` has passed since the last one was
    /// *claimed* — the claim is the floor between two probes on one machine, whatever the
    /// scheduler's tick spacing is.
    pub(crate) fn due(&self, interval: Duration, now: Instant) -> bool {
        let condition = self.lock();
        match condition.last_probe_at {
            None => true,
            Some(at) => now.saturating_duration_since(at) >= interval,
        }
    }

    /// Reports whether the member takes traffic under the active checker's judgment.
    pub(crate) fn admits(&self) -> bool {
        self.lock().status == Status::Healthy
    }

    /// Reports whether this member's active state needs the probe reset a config change asks for.
    ///
    /// Answered by comparing the check the state has been counting under with the one the
    /// document now publishes; equality is the whole rule.
    pub(crate) fn under(&self) -> Option<CheckFingerprint> {
        self.lock().under
    }

    /// Records which check the state's current counts belong to.
    pub(crate) fn counted_under(&self, fingerprint: CheckFingerprint) {
        self.lock().under = Some(fingerprint);
    }

    #[cfg(test)]
    pub(crate) fn status(&self) -> Status {
        self.lock().status
    }

    #[cfg(test)]
    pub(crate) fn in_flight(&self) -> bool {
        self.lock().in_flight
    }

    /// Locks the condition, recovering a poisoned one. Nothing under the lock can panic.
    fn lock(&self) -> MutexGuard<'_, Condition> {
        match self.condition.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// The identity of the check the current counts were counted under.
///
/// The fields are the ones that change what a count *means*: a threshold change re-reads the same
/// count differently, and an interval change says nothing about past verdicts but is part of the
/// same document block, so the fingerprint carries it as one identity rather than reasoning about
/// which fields matter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CheckFingerprint {
    unhealthy_threshold: u8,
    healthy_threshold: u8,
    interval_ms: u32,
    no_traffic_interval_ms: u32,
    timeout_ms: u32,
}

impl CheckFingerprint {
    /// Builds a fingerprint from raw parts, for tests and for callers that hold the values.
    #[cfg(test)]
    pub(crate) fn of_parts(
        unhealthy_threshold: u8,
        healthy_threshold: u8,
        interval_ms: u32,
        no_traffic_interval_ms: u32,
        timeout_ms: u32,
    ) -> Self {
        Self {
            unhealthy_threshold,
            healthy_threshold,
            interval_ms,
            no_traffic_interval_ms,
            timeout_ms,
        }
    }

    /// Reads the fingerprint of one document check.
    pub(crate) fn of(check: &nico_core::HealthCheck) -> Self {
        Self {
            unhealthy_threshold: check.unhealthy_threshold(),
            healthy_threshold: check.healthy_threshold(),
            interval_ms: check.interval_ms(),
            no_traffic_interval_ms: check.no_traffic_interval_ms(),
            timeout_ms: check.timeout_ms(),
        }
    }
}

/// The active state's own condition: what it has counted and where the member stands.
#[derive(Debug, Default)]
struct Condition {
    status: Status,
    consecutive_failures: u8,
    consecutive_successes: u8,
    in_flight: bool,
    under: Option<CheckFingerprint>,
    last_probe_at: Option<std::time::Instant>,
}

#[cfg(test)]
mod tests;
