//! The breaker's transition matrix and the table's identity rules.
//!
//! Every transition of the machine is asserted at least once, in both directions, plus the two
//! rules the module documentation promises: a lost probe is retried after one cooldown, and a
//! disabled breaker never refuses. The machine only ever compares instants, so the tests pass
//! explicit timestamps instead of driving a paused clock.

use std::time::Duration;

use tokio::time::Instant;

use super::*;
use crate::{provider::ProviderKind, route::TargetIdentity};

/// The cooldown the fixtures use, in the milliseconds the document spells.
const COOLDOWN_MS: u32 = 30_000;
const COOLDOWN: Duration = Duration::from_millis(30_000);

fn params(threshold: u32) -> BreakerParams {
    BreakerParams::new(threshold, COOLDOWN_MS)
}

fn identity(threshold: u32) -> TargetIdentity {
    TargetIdentity::new(
        "primary",
        "http://127.0.0.1:8080",
        ProviderKind::new("mock"),
        params(threshold),
    )
}

/// Drives one machine to `open` with consecutive failures, one second apart, and returns the instant
/// it opened — the one the cooldown is measured from.
fn break_it(breaker: &mut Breaker, threshold: u32, start: Instant) -> Instant {
    let mut now = start;
    for failure in 0..threshold {
        breaker.record(AttemptOutcome::Failure, None, now);
        // The last failure is the one that opened the breaker, so the clock stops there: advancing
        // past it would measure every cooldown from a second after the fact.
        if failure + 1 < threshold {
            now += Duration::from_secs(1);
        }
    }
    now
}

#[test]
fn a_closed_breaker_admits_until_the_threshold_is_reached() {
    let mut breaker = Breaker::new(params(3));
    let mut now = Instant::now();
    assert_eq!(breaker.admit(now), Admission::Admit);
    breaker.record(AttemptOutcome::Failure, None, now);
    now += Duration::from_secs(1);
    assert_eq!(
        breaker.admit(now),
        Admission::Admit,
        "one failure is not open"
    );
    breaker.record(AttemptOutcome::Failure, None, now);
    now += Duration::from_secs(1);
    assert_eq!(breaker.admit(now), Admission::Admit);
    breaker.record(AttemptOutcome::Failure, None, now);
    assert_eq!(
        breaker.phase(),
        BreakerPhase::Open,
        "the third consecutive failure opens"
    );
    assert_eq!(
        breaker.admit(now),
        Admission::Skip(BreakerPhase::Open),
        "an open breaker refuses"
    );
}

#[test]
fn a_success_resets_the_consecutive_failure_count() {
    let mut breaker = Breaker::new(params(3));
    let mut now = Instant::now();
    for _ in 0..2 {
        breaker.record(AttemptOutcome::Failure, None, now);
        now += Duration::from_secs(1);
    }
    breaker.record(AttemptOutcome::Success, None, now);
    now += Duration::from_secs(1);
    breaker.record(AttemptOutcome::Failure, None, now);
    assert_eq!(
        breaker.phase(),
        BreakerPhase::Closed,
        "the counter starts over after a success"
    );
}

#[test]
fn the_cooldown_expires_into_one_probe() {
    let mut breaker = Breaker::new(params(1));
    let opened = break_it(&mut breaker, 1, Instant::now());
    assert_eq!(
        breaker.admit(opened + COOLDOWN - Duration::from_millis(1)),
        Admission::Skip(BreakerPhase::Open),
        "a millisecond short of the cooldown is still the cooldown"
    );
    let probe = opened + COOLDOWN;
    assert_eq!(
        breaker.admit(probe),
        Admission::Admit,
        "the first request after the cooldown becomes the probe"
    );
    assert_eq!(breaker.phase(), BreakerPhase::HalfOpen);
    assert_eq!(
        breaker.admit(probe),
        Admission::Skip(BreakerPhase::HalfOpen),
        "while the probe is out, everyone else skips"
    );
    assert!(
        probe.duration_since(opened) >= COOLDOWN,
        "the probe is the request the cooldown admitted"
    );
}

#[test]
fn a_probe_success_closes_the_breaker() {
    let mut breaker = Breaker::new(params(1));
    let opened = break_it(&mut breaker, 1, Instant::now());
    let probe = opened + COOLDOWN;
    assert_eq!(breaker.admit(probe), Admission::Admit);
    breaker.record(AttemptOutcome::Success, None, probe);
    assert_eq!(breaker.phase(), BreakerPhase::Closed);
    assert_eq!(
        breaker.admit(probe + Duration::from_secs(1)),
        Admission::Admit,
        "a recovered target serves again"
    );
}

#[test]
fn a_probe_failure_reopens_the_breaker() {
    let mut breaker = Breaker::new(params(1));
    let opened = break_it(&mut breaker, 1, Instant::now());
    let probe = opened + COOLDOWN;
    assert_eq!(breaker.admit(probe), Admission::Admit);
    breaker.record(AttemptOutcome::Failure, None, probe);
    assert_eq!(breaker.phase(), BreakerPhase::Open);
    assert_eq!(
        breaker.admit(probe),
        Admission::Skip(BreakerPhase::Open),
        "the target must wait out its cooldown again"
    );
}

#[test]
fn a_lost_probe_is_retried_after_one_cooldown() {
    // An attempt whose outcome is lost (a disconnect, a deadline) would leave the breaker half-open
    // forever, and a recovered target would never be probed again. The machine admits a fresh probe
    // after one cooldown instead.
    let mut breaker = Breaker::new(params(1));
    let opened = break_it(&mut breaker, 1, Instant::now());
    let probe = opened + COOLDOWN;
    assert_eq!(breaker.admit(probe), Admission::Admit);
    assert_eq!(
        breaker.admit(probe + COOLDOWN),
        Admission::Admit,
        "the probe never reported; the next request becomes one"
    );
    assert_eq!(breaker.phase(), BreakerPhase::HalfOpen);
}

#[test]
fn a_failure_reported_while_open_does_not_extend_the_cooldown() {
    let mut breaker = Breaker::new(params(1));
    let opened = break_it(&mut breaker, 1, Instant::now());
    // An attempt admitted before the breaker opened reports late.
    breaker.record(
        AttemptOutcome::Failure,
        None,
        opened + Duration::from_secs(10),
    );
    assert_eq!(
        breaker.admit(opened + COOLDOWN),
        Admission::Admit,
        "the cooldown the operator set, measured from the moment the breaker opened"
    );
}

#[test]
fn a_disabled_breaker_counts_but_never_refuses() {
    let mut breaker = Breaker::new(params(0));
    let mut now = Instant::now();
    for _ in 0..10 {
        assert_eq!(breaker.admit(now), Admission::Admit);
        breaker.record(AttemptOutcome::Failure, None, now);
        now += Duration::from_secs(1);
    }
    assert_eq!(breaker.phase(), BreakerPhase::Closed);
}

#[test]
fn the_table_keeps_one_state_per_identity_and_inherits_across_questions() {
    let table = BreakerTable::new();
    let known = identity(2);
    let mut now = Instant::now();
    table.record(&known, AttemptOutcome::Failure, None, now);
    now += Duration::from_secs(1);
    table.record(&known, AttemptOutcome::Failure, None, now);
    assert_eq!(
        table.admit(&known, now),
        Admission::Skip(BreakerPhase::Open),
        "the state the first questions built is the state the next question reads"
    );
}

#[test]
fn changed_parameters_start_the_state_over() {
    // The parameters are part of the identity: a breaker that outlived its own thresholds would
    // open or recover under rules the document no longer states.
    let table = BreakerTable::new();
    let before = identity(1);
    let now = Instant::now();
    table.record(&before, AttemptOutcome::Failure, None, now);
    assert_eq!(
        table.admit(&before, now),
        Admission::Skip(BreakerPhase::Open)
    );

    let retuned = TargetIdentity::new(
        "primary",
        "http://127.0.0.1:8080",
        ProviderKind::new("mock"),
        params(5),
    );
    assert_eq!(
        table.admit(&retuned, now),
        Admission::Admit,
        "a re-tuned endpoint starts its machine over"
    );
}

#[test]
fn retain_drops_the_state_of_targets_no_live_identity_names() {
    let table = BreakerTable::new();
    let removed = identity(1);
    let kept = TargetIdentity::new(
        "secondary",
        "http://127.0.0.1:8080",
        ProviderKind::new("mock"),
        params(1),
    );
    let now = Instant::now();
    table.record(&removed, AttemptOutcome::Failure, None, now);
    table.record(&kept, AttemptOutcome::Failure, None, now);
    table.retain(|live| live.provider() == "secondary");
    assert_eq!(
        table.admit(&removed, now),
        Admission::Admit,
        "a removed target's history is gone"
    );
    assert_eq!(
        table.admit(&kept, now),
        Admission::Skip(BreakerPhase::Open),
        "a served target keeps its state"
    );
}
