//! The four strategies and the inheritance rule, exercised through the trait.
//!
//! Every test states a sequence, not a distribution: each picker is deterministic given its inputs,
//! so a test that accepted "roughly a third each" would be hiding the implementation behind a
//! statistic instead of asserting the behaviour the document configures.
//!
//! The fixtures are a fake [`Members`] impl rather than a document, because these tests are about the
//! rules alone: what a pool publishes and what a member observed are `pools`' tests.

use super::*;
use crate::IpHash;

/// A member table a test controls completely: weights, in-flight counts and names, no document.
#[derive(Debug, Default)]
struct Fixture {
    weights: Vec<u16>,
    active: Vec<u32>,
    names: Vec<Arc<str>>,
}

impl Fixture {
    /// Builds a fixture of `names`, all weight 1 and idle.
    fn named(names: &[&str]) -> Self {
        Self {
            weights: vec![1; names.len()],
            active: vec![0; names.len()],
            names: names.iter().map(|name| Arc::from(*name)).collect(),
        }
    }

    /// Sets the weights, in member order.
    fn with_weights(mut self, weights: &[u16]) -> Self {
        self.weights = weights.to_vec();
        self
    }

    /// Sets the in-flight counts, in member order.
    fn with_active(mut self, active: &[u32]) -> Self {
        self.active = active.to_vec();
        self
    }

    /// Returns the names of every member, as a picker's inheritance sees them.
    fn identities(&self) -> Vec<Arc<str>> {
        self.names.clone()
    }
}

impl Members for Fixture {
    fn len(&self) -> usize {
        self.names.len()
    }

    fn weight(&self, index: usize) -> u16 {
        self.weights.get(index).copied().unwrap_or_default()
    }

    fn active(&self, index: usize) -> u32 {
        self.active.get(index).copied().unwrap_or_default()
    }

    fn name(&self, index: usize) -> &str {
        self.names.get(index).map_or("", |name| name)
    }
}

/// Runs `count` selections against `fixture` with every member a candidate.
fn sequence(picker: &Arc<dyn Picker>, fixture: &Fixture, count: usize) -> Vec<Option<usize>> {
    let candidates: Vec<usize> = (0..fixture.len()).collect();
    let view = View::new(fixture, &candidates, None);
    (0..count).map(|_| picker.pick(&view)).collect()
}

#[test]
fn round_robin_walks_the_candidates_and_wraps() {
    let picker: Arc<dyn Picker> = Arc::new(RoundRobin::default());
    let fixture = Fixture::named(&["one", "two", "three"]);

    let chosen = sequence(&picker, &fixture, 5);

    assert_eq!(chosen, [Some(0), Some(1), Some(2), Some(0), Some(1)]);
}

#[test]
fn round_robin_answers_none_without_candidates() {
    let picker: Arc<dyn Picker> = Arc::new(RoundRobin::default());
    let fixture = Fixture::named(&["one"]);

    assert_eq!(picker.pick(&View::new(&fixture, &[], None)), None);
}

#[test]
fn smooth_weighted_interleaves_the_shares_instead_of_bursting() {
    // nginx's 3:1:1 sequence. The heavy member is not sent three times in a row: it takes the first,
    // third and fifth request, and the light members share what is left.
    let picker: Arc<dyn Picker> = Arc::new(SmoothWeighted::default());
    let fixture = Fixture::named(&["one", "two", "three"]).with_weights(&[3, 1, 1]);

    let chosen = sequence(&picker, &fixture, 5);

    assert_eq!(
        chosen,
        [Some(0), Some(1), Some(0), Some(2), Some(0)],
        "a full cycle must interleave, not burst"
    );
    assert_eq!(
        sequence(&picker, &fixture, 1),
        [Some(0)],
        "a full cycle leaves the running weights where they started, so the next choice repeats it"
    );
}

#[test]
fn smooth_weighted_counts_only_the_candidates_that_are_left() {
    // Member 2 is not a candidate any more. Its share of the total must not be redistributed against
    // the others: the total taken back off the winner is the sum of the *candidates'* weights.
    let picker: Arc<dyn Picker> = Arc::new(SmoothWeighted::default());
    let fixture = Fixture::named(&["one", "two", "three"]).with_weights(&[3, 1, 1]);
    let candidates = [0, 1];
    let view = View::new(&fixture, &candidates, None);

    let chosen: Vec<Option<usize>> = (0..4).map(|_| picker.pick(&view)).collect();

    assert_eq!(chosen, [Some(0), Some(0), Some(1), Some(0)]);
}

#[test]
fn least_connections_prefers_the_idle_member() {
    let picker: Arc<dyn Picker> = Arc::new(LeastConn::default());
    let fixture = Fixture::named(&["one", "two", "three"]).with_active(&[4, 0, 2]);

    assert_eq!(sequence(&picker, &fixture, 1), [Some(1)]);
}

#[test]
fn least_connections_breaks_ties_in_turn() {
    let picker: Arc<dyn Picker> = Arc::new(LeastConn::default());
    let fixture = Fixture::named(&["one", "two", "three"]).with_active(&[2, 2, 2]);

    assert_eq!(
        sequence(&picker, &fixture, 4),
        [Some(0), Some(1), Some(2), Some(0)]
    );
}

#[test]
fn least_connections_answers_none_without_candidates() {
    let picker: Arc<dyn Picker> = Arc::new(LeastConn::default());
    let fixture = Fixture::named(&["one"]);

    assert_eq!(picker.pick(&View::new(&fixture, &[], None)), None);
}
#[test]
fn round_robin_keeps_its_position_across_a_publication() {
    let first: Arc<dyn Picker> = Arc::new(RoundRobin::default());
    let fixture = Fixture::named(&["one", "two", "three"]);
    assert_eq!(sequence(&first, &fixture, 2), [Some(0), Some(1)]);

    let next = first.inherit(&fixture.identities(), &fixture.identities());

    assert_eq!(
        sequence(&next, &fixture, 1),
        [Some(2)],
        "a reload must not restart the rotation at the first member"
    );
}

#[test]
fn smooth_weighted_carries_running_weights_only_for_kept_identities() {
    let first: Arc<dyn Picker> = Arc::new(SmoothWeighted::default());
    let fixture = Fixture::named(&["one", "two", "three"]).with_weights(&[3, 1, 1]);
    // Walk most of a cycle, so the running weights are somewhere a fresh picker would not be.
    let _ = sequence(&first, &fixture, 3);

    // The document keeps "one", renames the second member and adds a third. Only "one" may carry its
    // running weight; the others start at zero.
    let after = Fixture::named(&["one", "renamed", "fresh"]).with_weights(&[3, 1, 1]);
    let inherited = first.inherit(&fixture.identities(), &after.identities());

    // Replaying from a fresh picker and from the inherited one must reach the same member for the
    // first step: "one" is where it was, and the two newcomers start level.
    let fresh = Arc::new(SmoothWeighted::default());
    assert_eq!(
        inherited.pick(&View::new(&after, &[0, 1, 2], None)),
        fresh.pick(&View::new(&after, &[0, 1, 2], None)),
        "a renamed member must not inherit another machine's running weight"
    );
}

#[test]
fn a_strategy_change_is_not_inherited() {
    // The decision belongs to `pools`, which only calls `inherit` when the document kept its
    // strategy. What the trait guarantees is that a picker's `inherit` returns a picker of its own
    // kind, so a rotation keeps rotating whatever the names were.
    let round_robin: Arc<dyn Picker> = Arc::new(RoundRobin::default());
    let fixture = Fixture::named(&["one", "two", "three"]);

    let inherited = round_robin.inherit(&fixture.identities(), &fixture.identities());

    assert_eq!(
        sequence(&inherited, &fixture, 3),
        [Some(0), Some(1), Some(2)],
        "a rotation keeps rotating, whatever the names were"
    );
}

#[test]
fn every_strategy_answers_one_of_its_candidates() {
    // The containment rule the trait states: a picker never names a member outside the set it was
    // handed, whatever the weights, loads or names are.
    let fixture = Fixture::named(&["one", "two", "three"]).with_weights(&[1, 5, 2]);
    let candidates = [1usize, 2];
    let pickers: Vec<(&str, Arc<dyn Picker>)> = vec![
        ("round_robin", Arc::new(RoundRobin::default())),
        ("smooth_weighted", Arc::new(SmoothWeighted::default())),
        ("least_conn", Arc::new(LeastConn::default())),
        ("ip_hash", Arc::new(IpHash::default())),
    ];
    for (strategy, picker) in pickers {
        for _ in 0..8 {
            let chosen = picker.pick(&View::new(&fixture, &candidates, None));
            assert!(
                chosen.is_some_and(|index| candidates.contains(&index)),
                "{strategy} chose {chosen:?}, outside {candidates:?}"
            );
        }
    }
}
