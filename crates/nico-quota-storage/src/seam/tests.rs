//! The seam's semantics: expiry, reset, the limit verdict, dedupe, and the corrupt refusals.

use std::time::{Duration, SystemTime};

use nico_http::{BudgetPolicy, ChargeId};

use super::{
    BackendError, CheckOutcome, RETAINED_CHARGE_IDS, WindowRecord, apply_charge, apply_check,
};

fn policy(limit: u64, window: u32) -> BudgetPolicy {
    BudgetPolicy {
        limit_tokens: limit,
        window_seconds: window,
    }
}

fn record(used: u64, started: SystemTime, charges: &[&str]) -> WindowRecord {
    WindowRecord {
        used,
        started,
        charges: charges.iter().map(|id| ChargeId((*id).into())).collect(),
    }
}

#[test]
fn an_absent_window_admits_and_opens_a_fresh_one() {
    let now = SystemTime::UNIX_EPOCH;
    let applied =
        apply_check(None, &policy(100, 60), now).expect("an absent window is not corrupt");

    assert_eq!(applied.outcome, CheckOutcome::Allowed);
    let stored = applied
        .stored
        .expect("an allowed check stores the fresh window");
    assert_eq!(stored.used, 0);
    assert_eq!(stored.started, now);
    assert!(stored.charges.is_empty());
}

#[test]
fn an_expired_window_is_treated_as_absent() {
    let now = SystemTime::UNIX_EPOCH;
    let stored = record(1_000, now, &[]);
    let later = now + Duration::from_secs(61);

    let applied = apply_check(Some(stored), &policy(1_000, 60), later)
        .expect("an expired window is not corrupt");

    assert_eq!(applied.outcome, CheckOutcome::Allowed);
    let fresh = applied.stored.expect("the expired window is replaced");
    assert_eq!(
        fresh.started, later,
        "the fresh window opens at the check's time"
    );
}

#[test]
fn a_live_spent_window_refuses_without_restoring_anything() {
    let now = SystemTime::UNIX_EPOCH;
    let stored = record(1_000, now, &[]);

    let applied = apply_check(
        Some(stored),
        &policy(1_000, 60),
        now + Duration::from_secs(30),
    )
    .expect("a live window is not corrupt");

    assert_eq!(applied.outcome, CheckOutcome::Spent);
    assert!(applied.stored.is_none(), "a refusal writes nothing");
}

#[test]
fn a_live_window_with_room_admits_and_touches_the_record() {
    let now = SystemTime::UNIX_EPOCH;
    let stored = record(600, now, &["c1"]);

    let applied = apply_check(
        Some(stored.clone()),
        &policy(1_000, 60),
        now + Duration::from_secs(1),
    )
    .expect("a live window is not corrupt");

    assert_eq!(applied.outcome, CheckOutcome::Allowed);
    assert_eq!(
        applied.stored,
        Some(stored),
        "an allowed check keeps the record"
    );
}

#[test]
fn a_charge_on_an_expired_window_opens_a_fresh_one_with_the_full_amount() {
    let now = SystemTime::UNIX_EPOCH;
    let stored = record(1_000, now, &["old"]);
    let later = now + Duration::from_secs(61);

    let applied = apply_charge(
        Some(stored),
        &policy(1_000, 60),
        &ChargeId("new".into()),
        800,
        later,
    )
    .expect("an expired window is not corrupt");

    assert!(applied.first_write);
    assert_eq!(
        applied.stored.used, 800,
        "the fresh window records the full charge"
    );
    assert_eq!(applied.stored.started, later);
    assert_eq!(applied.stored.charges, vec![ChargeId("new".into())]);
}

#[test]
fn a_duplicate_charge_id_records_nothing_new() {
    let now = SystemTime::UNIX_EPOCH;
    let stored = record(300, now, &["same"]);

    let applied = apply_charge(
        Some(stored),
        &policy(1_000, 60),
        &ChargeId("same".into()),
        300,
        now,
    )
    .expect("a live window is not corrupt");

    assert!(!applied.first_write, "the retry is recognized");
    assert_eq!(
        applied.stored.used, 300,
        "the retried charge did not bill twice"
    );
    assert_eq!(applied.stored.charges, vec![ChargeId("same".into())]);
}

#[test]
fn a_charge_over_the_limit_is_still_recorded() {
    let now = SystemTime::UNIX_EPOCH;
    let stored = record(900, now, &[]);

    let applied = apply_charge(
        Some(stored),
        &policy(1_000, 60),
        &ChargeId("big".into()),
        500,
        now,
    )
    .expect("a live window is not corrupt");

    assert_eq!(
        applied.stored.used, 1_400,
        "post-hoc accounting records what the provider served"
    );
}

#[test]
fn the_charge_id_history_is_bounded() {
    let now = SystemTime::UNIX_EPOCH;
    let mut stored = WindowRecord {
        used: 0,
        started: now,
        charges: Vec::new(),
    };

    // Fill the history past its bound, then confirm the oldest ids fall off.
    for index in 0..RETAINED_CHARGE_IDS + 10 {
        let applied = apply_charge(
            Some(stored),
            &policy(1_000_000, 60),
            &ChargeId(format!("c{index}").into()),
            1,
            now,
        )
        .expect("a live window is not corrupt");
        stored = applied.stored;
    }

    assert_eq!(stored.charges.len(), RETAINED_CHARGE_IDS);
    assert_eq!(
        stored.charges.first().map(|id| id.0.as_ref()),
        Some("c10"),
        "the oldest ids were forgotten, newest retained"
    );
}

#[test]
fn a_negative_clock_skew_does_not_expire_a_live_window() {
    let now = SystemTime::UNIX_EPOCH;
    let stored = record(500, now + Duration::from_secs(5), &[]);

    let applied = apply_check(Some(stored), &policy(1_000, 60), now)
        .expect("a window opened slightly ahead of the local clock is still readable");

    assert_eq!(
        applied.outcome,
        CheckOutcome::Allowed,
        "clock skew does not expire the window"
    );
}

#[test]
fn backend_errors_carry_typed_causes_into_the_domain() {
    let cause: Box<dyn std::error::Error + Send + Sync> = "the wire broke".into();
    let domain = BackendError::Unavailable(cause).into_domain();

    assert!(
        matches!(domain, nico_http::BudgetStoreError::Unavailable { .. }),
        "an unavailable backend folds into the domain's unavailable variant"
    );
}
