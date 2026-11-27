//! Fall, rise, the single slot, the reset-on-config-change, and the initial verdict.

use super::{ActiveHealth, ActiveTransition, CheckFingerprint, ProbeOutcome, Status};

fn fingerprint(fall: u8, rise: u8) -> CheckFingerprint {
    CheckFingerprint::of_parts(fall, rise, 5_000, 30_000, 1_000)
}

#[test]
fn a_fresh_member_is_healthy_with_nothing_in_flight() {
    let active = ActiveHealth::new();
    assert_eq!(active.status(), Status::Healthy);
    assert!(!active.in_flight());
    assert!(active.admits());
}

#[test]
fn the_first_failed_probe_below_the_threshold_does_not_move_the_status() {
    let active = ActiveHealth::new();
    assert!(active.begin_probe(false));
    let transition = active.finish_probe(ProbeOutcome::Failed, 3, 1);
    assert_eq!(transition, ActiveTransition::None);
    assert_eq!(
        active.status(),
        Status::Healthy,
        "1 of 3 failures: no verdict yet"
    );
    assert!(!active.in_flight(), "the report released the slot");
}

#[test]
fn the_fall_threshold_takes_the_member_out() {
    let active = ActiveHealth::new();
    active.begin_probe(false);
    assert_eq!(
        active.finish_probe(ProbeOutcome::Failed, 3, 1),
        ActiveTransition::None
    );
    active.begin_probe(false);
    assert_eq!(
        active.finish_probe(ProbeOutcome::Failed, 3, 1),
        ActiveTransition::None
    );
    active.begin_probe(false);
    assert_eq!(
        active.finish_probe(ProbeOutcome::Failed, 3, 1),
        ActiveTransition::Fell,
        "the third consecutive failure is the verdict"
    );
    assert_eq!(active.status(), Status::Unhealthy);
    assert!(!active.admits());
}

#[test]
fn a_passed_probe_resets_the_failure_count() {
    let active = ActiveHealth::new();
    for _ in 0..2 {
        active.begin_probe(false);
        active.finish_probe(ProbeOutcome::Failed, 3, 1);
    }
    active.begin_probe(false);
    active.finish_probe(ProbeOutcome::Passed, 3, 1);
    // The pass cleared the streak: two more failures are still below 3.
    for _ in 0..2 {
        active.begin_probe(false);
        assert_eq!(
            active.finish_probe(ProbeOutcome::Failed, 3, 1),
            ActiveTransition::None
        );
    }
    assert_eq!(active.status(), Status::Healthy);
}

#[test]
fn the_rise_threshold_returns_the_member() {
    let active = ActiveHealth::new();
    for _ in 0..2 {
        active.begin_probe(false);
        active.finish_probe(ProbeOutcome::Failed, 2, 3);
    }
    assert_eq!(active.status(), Status::Unhealthy);

    for _ in 0..2 {
        active.begin_probe(false);
        assert_eq!(
            active.finish_probe(ProbeOutcome::Passed, 2, 3),
            ActiveTransition::None,
            "1 and 2 of 3 passes: no verdict yet"
        );
    }
    active.begin_probe(false);
    assert_eq!(
        active.finish_probe(ProbeOutcome::Passed, 2, 3),
        ActiveTransition::Rose,
        "the third consecutive pass returns the member"
    );
    assert!(active.admits());
}

#[test]
fn a_failed_probe_resets_the_rise_count() {
    let active = ActiveHealth::new();
    for _ in 0..2 {
        active.begin_probe(false);
        active.finish_probe(ProbeOutcome::Failed, 2, 3);
    }
    // One pass, then a failure: the rise count restarts.
    active.begin_probe(false);
    active.finish_probe(ProbeOutcome::Passed, 2, 3);
    active.begin_probe(false);
    active.finish_probe(ProbeOutcome::Failed, 2, 3);
    for _ in 0..2 {
        active.begin_probe(false);
        assert_eq!(
            active.finish_probe(ProbeOutcome::Passed, 2, 3),
            ActiveTransition::None
        );
    }
    assert_eq!(
        active.status(),
        Status::Unhealthy,
        "2 of 3: the failed probe reset the rise"
    );
}

#[test]
fn a_second_probe_is_refused_while_one_is_out() {
    let active = ActiveHealth::new();
    assert!(active.begin_probe(false));
    assert!(
        !active.begin_probe(false),
        "one member, one probe at a time"
    );
    active.finish_probe(ProbeOutcome::Passed, 1, 1);
    assert!(
        active.begin_probe(false),
        "the released slot admits the next probe"
    );
}

#[test]
fn a_reset_restarts_from_healthy_with_empty_counts() {
    let active = ActiveHealth::new();
    for _ in 0..2 {
        active.begin_probe(false);
        active.finish_probe(ProbeOutcome::Failed, 2, 3);
    }
    assert_eq!(active.status(), Status::Unhealthy);

    // The document changed the check: the new checker starts from its own zero.
    assert!(active.begin_probe(true));
    active.finish_probe(ProbeOutcome::Passed, 2, 3);
    assert_eq!(
        active.status(),
        Status::Healthy,
        "the reset cleared the old fall"
    );
}

#[test]
fn the_fingerprint_changes_when_the_check_changes() {
    let active = ActiveHealth::new();
    assert_eq!(
        active.under(),
        None,
        "a fresh state has no check to remember"
    );
    active.counted_under(fingerprint(2, 3));
    assert_eq!(active.under(), Some(fingerprint(2, 3)));
    assert_ne!(
        fingerprint(2, 3),
        fingerprint(3, 3),
        "a threshold change is a different check"
    );
}
