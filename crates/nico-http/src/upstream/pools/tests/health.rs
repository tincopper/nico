//! The ejection state machine, driven through one generation's member list.
//!
//! The machine has four states and one transition between each pair, and every test here states one
//! of them: a streak that reaches the budget, a cooldown that expires, the single probe that comes
//! with it, and the answer that either keeps the member out or brings it back. The clock is an
//! argument everywhere, so a test asserts the transition instead of sleeping until it happens.
//!
//! Two of the rules are easy to get wrong and have their own tests: a probe that never reports must
//! not strand the member forever, and a failure reported for an attempt admitted *before* the ejection
//! must not extend the cooldown.

use std::time::{Duration, Instant};

use super::*;
use crate::upstream::{
    dns::ExpandedMembers,
    pools::{MemberFailure, MemberOutcome},
};

/// [`THREE_MEMBERS`] with a short cooldown, so a test can step past it without arithmetic on the
/// default thirty seconds.
const BUDGETED_MEMBERS: &str = concat!(
    "  - name: api-backend\n",
    "    scheme: http\n",
    "    host: api.example.test\n",
    "    port: 8080\n",
    "    max_fails: 2\n",
    "    fail_timeout_ms: 1000\n",
    "    endpoints:\n",
    "      - name: one\n",
    "        address: 10.0.0.1\n",
    "      - name: two\n",
    "        address: 10.0.0.2\n",
    "      - name: three\n",
    "        address: 10.0.0.3\n",
);

/// The upstream's failure budget, as the document above states it.
const COOLDOWN: Duration = Duration::from_millis(1000);

/// Returns the fixture's published state, at a fixed instant the test steps forward from.
fn budgeted() -> (Arc<UpstreamState>, Instant) {
    let pools = UpstreamPools::default();
    let table = pools.publish(&published(BUDGETED_MEMBERS), &ExpandedMembers::default());
    (Arc::clone(upstream(&table)), Instant::now())
}

/// Reports one connection failure against `member`.
///
/// Reporting through a guard would need a selection first, which is exactly what an ejected member
/// refuses; the tests that care about the *selection* use [`select_at`] instead.
fn fail(state: &UpstreamState, index: usize, now: Instant) {
    state.members()[index].state().record(
        state.ejection(),
        MemberOutcome::Failed(MemberFailure::Connect),
        now,
    );
}

/// Reports one answer from `member`.
fn answer(state: &UpstreamState, index: usize, now: Instant) {
    state.members()[index]
        .state()
        .record(state.ejection(), MemberOutcome::Answered, now);
}

/// Returns the addresses a selection sequence reaches, for the tests that assert which members are
/// still candidates.
fn reachable(state: &UpstreamState, now: Instant, requests: usize) -> Vec<String> {
    (0..requests)
        .filter_map(|_| {
            state
                .select_excluding(&[], None, now)
                .map(|selected| selected.destination.host().to_owned())
        })
        .collect()
}

#[test]
fn a_member_leaves_rotation_when_its_streak_reaches_the_budget() {
    let (state, now) = budgeted();

    fail(&state, 0, now);
    assert!(
        reachable(&state, now, 3).contains(&"10.0.0.1".to_owned()),
        "one failure is below the budget of two"
    );
    fail(&state, 0, now);
    assert_eq!(state.members()[0].state().failures(), 2);

    let reached = reachable(&state, now, 6);
    assert!(
        !reached.contains(&"10.0.0.1".to_owned()),
        "an ejected member must not be a candidate, reached {reached:?}"
    );
    assert_eq!(
        reached.len(),
        6,
        "the other two members still serve every request"
    );
}

#[test]
fn an_answer_clears_the_streak_before_the_budget_is_reached() {
    let (state, now) = budgeted();

    fail(&state, 0, now);
    answer(&state, 0, now);
    assert_eq!(
        state.members()[0].state().failures(),
        0,
        "a 4xx is a caller's fault, but it still proves the member answered"
    );
    fail(&state, 0, now);
    assert!(
        reachable(&state, now, 3).contains(&"10.0.0.1".to_owned()),
        "the streak restarted, so one failure is once again below the budget"
    );
}

#[test]
fn an_expired_cooldown_admits_exactly_one_probe() {
    let (state, now) = budgeted();
    fail(&state, 0, now);
    fail(&state, 0, now);

    // Still cooling: nothing reaches the member.
    assert!(
        !state.members()[0]
            .state()
            .available(state.ejection(), now + COOLDOWN / 2)
    );

    let later = now + COOLDOWN;
    assert!(
        state.members()[0].state().admit(state.ejection(), later),
        "the first request after the cooldown becomes the probe"
    );
    assert!(
        !state.members()[0]
            .state()
            .available(state.ejection(), later),
        "and every other request keeps skipping the member while that probe is out"
    );
}

#[test]
fn a_selection_that_passes_over_the_member_does_not_spend_its_probe() {
    let (state, now) = budgeted();
    fail(&state, 0, now);
    fail(&state, 0, now);

    // Two selections while the member is out of rotation put the rotation one place past it, so the
    // first request after the cooldown is routed to another member.
    assert_eq!(
        reachable(&state, now, 2),
        ["10.0.0.2", "10.0.0.3"],
        "the ejected member is not a candidate yet"
    );

    let later = now + COOLDOWN;
    assert_eq!(
        reachable(&state, later, 1),
        ["10.0.0.3"],
        "the cooled-down member is a candidate again, but the rotation did not reach it"
    );
    assert!(
        state.members()[0]
            .state()
            .available(state.ejection(), later),
        "a probe that was never sent is not a probe that was spent"
    );
    assert!(
        reachable(&state, later, 3).contains(&"10.0.0.1".to_owned()),
        "the request that does reach the member is its probe, one cooldown after the ejection"
    );
}

#[test]
fn a_failed_probe_restarts_the_cooldown() {
    let (state, now) = budgeted();
    fail(&state, 0, now);
    fail(&state, 0, now);
    let probe_at = now + COOLDOWN;

    let probed = select_at(&state, probe_at);
    assert_eq!(
        probed.destination.host(),
        "10.0.0.1",
        "the ejected member is the one admitted as the probe"
    );
    probed
        .guard
        .record(MemberOutcome::Failed(MemberFailure::Status), probe_at);

    assert!(
        !state.members()[0]
            .state()
            .available(state.ejection(), probe_at + COOLDOWN / 2),
        "a probe that failed puts the member back for another full cooldown"
    );
    assert!(
        state.members()[0]
            .state()
            .available(state.ejection(), probe_at + COOLDOWN),
        "and it is probed again once that cooldown is spent"
    );
}

#[test]
fn a_probe_that_never_reports_is_retried_after_one_cooldown() {
    let (state, now) = budgeted();
    fail(&state, 0, now);
    fail(&state, 0, now);

    // The probe is claimed and the request ends without reporting: the client disconnected, the
    // gateway began shutting down, the budget expired.
    let orphaned = now + COOLDOWN;
    assert!(state.members()[0].state().admit(state.ejection(), orphaned));

    assert!(
        !state.members()[0]
            .state()
            .available(state.ejection(), orphaned + COOLDOWN / 2),
        "while that probe may still be out, nobody else is admitted"
    );
    assert!(
        state.members()[0]
            .state()
            .available(state.ejection(), orphaned + COOLDOWN),
        "a member whose probe never reported must be asked again, not stranded for the process's life"
    );
}

#[test]
fn a_late_answer_returns_the_member_without_waiting_for_its_probe() {
    let (state, now) = budgeted();
    fail(&state, 0, now);
    fail(&state, 0, now);

    // A request admitted before the ejection answers afterwards. A late *failure* is ignored because
    // it says nothing new; a late answer is evidence the machine is serving, so it closes the member
    // immediately instead of leaving it out for a cooldown nothing is wrong with.
    answer(&state, 0, now + COOLDOWN / 4);

    assert_eq!(state.members()[0].state().failures(), 0);
    assert!(
        reachable(&state, now + COOLDOWN / 4, 3).contains(&"10.0.0.1".to_owned()),
        "an answer is evidence, so the member serves again without waiting for the cooldown"
    );
}

#[test]
fn an_answered_probe_returns_the_member_to_rotation() {
    let (state, now) = budgeted();
    fail(&state, 0, now);
    fail(&state, 0, now);

    let probed = select_at(&state, now + COOLDOWN);
    probed.guard.record(MemberOutcome::Answered, now + COOLDOWN);

    assert_eq!(state.members()[0].state().failures(), 0);
    let reached = reachable(&state, now + COOLDOWN, 3);
    let mut distinct = reached.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(
        distinct,
        ["10.0.0.1", "10.0.0.2", "10.0.0.3"],
        "a recovered member is an ordinary candidate again: {reached:?}"
    );
}

#[test]
fn a_failure_reported_after_the_ejection_does_not_extend_the_cooldown() {
    let (state, now) = budgeted();
    fail(&state, 0, now);
    fail(&state, 0, now);

    // A request that was admitted while the member was still closed reports its failure later. It
    // says nothing new, and letting it move the ejection instant would let one slow burst keep the
    // machine out for as long as the burst lasts.
    fail(&state, 0, now + COOLDOWN / 4);
    assert!(
        state.members()[0]
            .state()
            .available(state.ejection(), now + COOLDOWN),
        "the cooldown still ends one fail_timeout after the ejection"
    );
}

#[test]
fn zero_max_fails_disables_ejection() {
    let pools = UpstreamPools::default();
    // `max_fails: 0` is the document's spelling for "never eject": the members are not
    // interchangeable endpoints but a fixed assignment, so no streak may take one out.
    let fixture = BUDGETED_MEMBERS.replace("max_fails: 2", "max_fails: 0");
    let table = pools.publish(&published(&fixture), &ExpandedMembers::default());
    let state = upstream(&table);
    let now = Instant::now();

    for _ in 0..10 {
        fail(state, 0, now);
    }

    assert_eq!(
        state.members()[0].state().failures(),
        10,
        "the streak is still counted; only the verdict is disabled"
    );
    assert!(
        reachable(state, now, 3).contains(&"10.0.0.1".to_owned()),
        "a member of an upstream with ejection disabled is always a candidate"
    );
}

#[test]
fn every_member_ejected_selects_nothing() {
    let (state, now) = budgeted();
    for index in 0..3 {
        fail(&state, index, now);
        fail(&state, index, now);
    }

    assert!(
        state.select_excluding(&[], None, now).is_none(),
        "an upstream with no available member answers `None`, which the data plane refuses"
    );
}

#[test]
fn a_failure_streak_survives_a_publication_and_is_judged_by_the_new_budget() {
    let pools = UpstreamPools::default();
    let first = pools.publish(&published(BUDGETED_MEMBERS), &ExpandedMembers::default());
    let now = Instant::now();
    fail(upstream(&first), 0, now);

    // The same member identity, re-published with a tighter budget: what the gateway observed about
    // the machine is a fact, and the budget is how it is judged. The judgment is applied to the next
    // outcome rather than retroactively — a member is ejected by a failure, never by a reload, so a
    // document that lowers the threshold does not take a serving machine out of rotation by itself.
    let tightened = BUDGETED_MEMBERS.replace("max_fails: 2", "max_fails: 1");
    let second = pools.publish(&published(&tightened), &ExpandedMembers::default());
    let inherited = upstream(&second);

    assert_eq!(
        inherited.members()[0].state().failures(),
        1,
        "the streak is inherited with the identity"
    );
    assert!(
        inherited.members()[0]
            .state()
            .available(inherited.ejection(), now),
        "a publication is not an observation: the member is still serving"
    );

    fail(inherited, 0, now);
    assert!(
        !inherited.members()[0]
            .state()
            .available(inherited.ejection(), now),
        "the inherited streak counts towards the new budget on the next failure"
    );
}

#[test]
fn a_member_whose_address_changed_starts_healthy() {
    let pools = UpstreamPools::default();
    let first = pools.publish(&published(BUDGETED_MEMBERS), &ExpandedMembers::default());
    let now = Instant::now();
    for _ in 0..2 {
        fail(upstream(&first), 0, now);
    }

    let moved = BUDGETED_MEMBERS.replace("10.0.0.1", "10.0.0.9");
    let second = pools.publish(&published(&moved), &ExpandedMembers::default());
    let replaced = upstream(&second);

    assert_eq!(
        replaced.members()[0].state().failures(),
        0,
        "another machine is another member, however it is named"
    );
    assert!(
        replaced.members()[0]
            .state()
            .available(replaced.ejection(), now),
        "a machine that was just written in has not failed anything"
    );
}
