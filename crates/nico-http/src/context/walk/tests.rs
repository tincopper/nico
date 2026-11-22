//! The walk's budget arithmetic: what one failed round spends, and where the walk stops.

use std::sync::Arc;

use super::*;

/// The member identity a test's selections produce; the walk only counts and collects them.
fn member(name: &str) -> MemberId {
    MemberId::new(Arc::from(name), Arc::from("10.0.0.1"), 8080)
}

#[test]
fn a_failed_round_spends_one_attempt_of_the_budget() {
    // `budget: 3` is three members including the first: two failures are survivable, the third is
    // the walk's end.
    let mut walk = TrafficWalk::new(Arc::from("api"), 3);
    assert!(walk.failed(), "the first failure leaves one more member");
    assert!(walk.failed(), "the second failure spends the last attempt");
    assert!(!walk.failed(), "the budget is spent");
    assert!(!walk.can_retry(), "the spent walk holds nothing");
}

#[test]
fn a_budget_of_one_is_the_shape_every_route_had_before_failover() {
    let mut walk = TrafficWalk::new(Arc::from("api"), 1);
    assert!(!walk.failed(), "one attempt, no retry");
}

#[test]
fn tried_members_collect_in_attempt_order() {
    let mut walk = TrafficWalk::new(Arc::from("api"), 3);
    walk.record_tried(member("one"));
    walk.record_tried(member("two"));
    let tried: Vec<String> = walk.tried().iter().map(|id| id.name().to_owned()).collect();
    assert_eq!(
        tried,
        ["one", "two"],
        "the walk remembers who it met, in order"
    );
}
