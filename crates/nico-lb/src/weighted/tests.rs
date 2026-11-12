//! The two distribution strategies: one keyed on the caller, one drawn from the weights.
//!
//! The draw's tests inject a deterministic [`Source`], so the sequence each test states is the
//! behaviour the document configured rather than a statistic that would hide a wrong implementation.

use std::net::{IpAddr, Ipv4Addr};

use super::*;
use crate::{Members, source::Source};

/// A source that returns a fixed sequence, cycling when it runs out.
#[derive(Debug)]
struct Scripted {
    values: Vec<u64>,
    position: std::sync::Mutex<usize>,
}

impl Scripted {
    fn new(values: Vec<u64>) -> Self {
        Self {
            values,
            position: std::sync::Mutex::new(0),
        }
    }
}

impl Source for Scripted {
    fn below(&self, total: u64) -> u64 {
        let mut position = self
            .position
            .lock()
            .expect("the scripted source lock is not poisoned");
        let value = self.values[*position % self.values.len()];
        *position += 1;
        value % total
    }
}

/// A member table a test controls completely.
#[derive(Debug, Default)]
struct Fixture {
    weights: Vec<u16>,
    names: Vec<std::sync::Arc<str>>,
}

impl Fixture {
    fn named(names: &[&str]) -> Self {
        Self {
            weights: vec![1; names.len()],
            names: names
                .iter()
                .map(|name| std::sync::Arc::from(*name))
                .collect(),
        }
    }

    fn with_weights(mut self, weights: &[u16]) -> Self {
        self.weights = weights.to_vec();
        self
    }
}

impl Members for Fixture {
    fn len(&self) -> usize {
        self.names.len()
    }

    fn weight(&self, index: usize) -> u16 {
        self.weights.get(index).copied().unwrap_or_default()
    }

    fn active(&self, _index: usize) -> u32 {
        0
    }

    fn name(&self, index: usize) -> &str {
        self.names.get(index).map_or("", |name| name)
    }
}

/// Runs `count` selections for one caller address, with every member a candidate.
fn sequence_for(
    picker: &Arc<dyn Picker>,
    fixture: &Fixture,
    client: IpAddr,
    count: usize,
) -> Vec<Option<usize>> {
    let candidates: Vec<usize> = (0..fixture.len()).collect();
    let view = View::new(fixture, &candidates, Some(client));
    (0..count).map(|_| picker.pick(&view)).collect()
}

/// Runs `count` selections with no caller address, with every member a candidate.
fn sequence(picker: &Arc<dyn Picker>, fixture: &Fixture, count: usize) -> Vec<Option<usize>> {
    let candidates: Vec<usize> = (0..fixture.len()).collect();
    let view = View::new(fixture, &candidates, None);
    (0..count).map(|_| picker.pick(&view)).collect()
}

#[test]
fn ip_hash_is_stable_for_one_caller() {
    let picker: Arc<dyn Picker> = Arc::new(IpHash::default());
    let fixture = Fixture::named(&["one", "two", "three"]);
    let caller = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7));

    let chosen = sequence_for(&picker, &fixture, caller, 4);

    assert!(
        chosen.windows(2).all(|pair| pair[0] == pair[1]),
        "one caller must stay on one member, went to {chosen:?}"
    );
    assert!(chosen[0].is_some());
}

#[test]
fn ip_hash_moves_only_the_keys_a_new_member_takes() {
    // 1000 caller addresses over three members, then the same addresses over four. Rendezvous hashing
    // is chosen over `hash % len` for exactly this: modulo remaps almost every key when the divisor
    // changes, while this rule moves about 1/n. An existing member's key must never move unless the
    // new member wins it.
    let picker: Arc<dyn Picker> = Arc::new(IpHash::default());
    let three = Fixture::named(&["one", "two", "three"]);
    let four = Fixture::named(&["one", "two", "three", "four"]);
    let candidates_three: Vec<usize> = (0..3).collect();
    let candidates_four: Vec<usize> = (0..4).collect();
    let mut moved = 0;
    for index in 0..1000u32 {
        let caller = IpAddr::V4(Ipv4Addr::from(index.to_be_bytes()));
        let before = picker.pick(&View::new(&three, &candidates_three, Some(caller)));
        let after = picker.pick(&View::new(&four, &candidates_four, Some(caller)));
        // Indices below three name the same machines; index 3 is the member that was just added.
        if after != Some(3) && before != after {
            moved += 1;
        }
    }

    assert_eq!(
        moved, 0,
        "an existing key must not move unless the new member takes it"
    );
}

#[test]
fn ip_hash_answers_none_without_candidates() {
    let picker: Arc<dyn Picker> = Arc::new(IpHash::default());
    let fixture = Fixture::named(&["one"]);
    let caller = IpAddr::V4(Ipv4Addr::LOCALHOST);

    assert_eq!(picker.pick(&View::new(&fixture, &[], Some(caller))), None);
}

#[test]
fn ip_hash_spreads_callers_without_an_address() {
    // No peer address (a Unix socket) means no key to be sticky on. Sending every such caller to one
    // member would be a pile-up, so the fallback rotates instead.
    let picker: Arc<dyn Picker> = Arc::new(IpHash::default());
    let fixture = Fixture::named(&["one", "two", "three"]);

    let chosen = sequence(&picker, &fixture, 4);

    assert_eq!(
        chosen,
        [Some(0), Some(1), Some(2), Some(0)],
        "a keyless request must still spread across the members"
    );
}

#[test]
fn ip_hash_spreads_distinct_callers_across_every_member() {
    let picker: Arc<dyn Picker> = Arc::new(IpHash::default());
    let fixture = Fixture::named(&["one", "two", "three"]);
    let candidates: Vec<usize> = (0..3).collect();
    let mut reached = std::collections::BTreeSet::new();
    for index in 0..64u32 {
        let caller = IpAddr::V4(Ipv4Addr::from(index.to_be_bytes()));
        reached.insert(picker.pick(&View::new(&fixture, &candidates, Some(caller))));
    }

    assert_eq!(
        reached.len(),
        3,
        "64 callers should reach every member, reached {reached:?}"
    );
}

#[test]
fn weighted_random_follows_the_weights() {
    // 3:1 over a range of 4. The draws are the points in that range, so the sequence states exactly
    // which slots belong to which member instead of leaving it to a statistic.
    let source = Arc::new(Scripted::new(vec![0, 1, 2, 3]));
    let picker: Arc<dyn Picker> = Arc::new(WeightedRandom::new(source));
    let fixture = Fixture::named(&["heavy", "light"]).with_weights(&[3, 1]);

    assert_eq!(
        sequence(&picker, &fixture, 4),
        [Some(0), Some(0), Some(0), Some(1)],
        "the heavy member owns the first three points of the range"
    );
}

#[test]
fn a_zero_weight_member_is_never_drawn() {
    // A member with no share contributes no width, so it cannot be reached however many draws happen.
    let source = Arc::new(Scripted::new((0..=10).collect()));
    let picker: Arc<dyn Picker> = Arc::new(WeightedRandom::new(source));
    let fixture = Fixture::named(&["fallback-only", "serving"]).with_weights(&[0, 1]);

    let chosen = sequence(&picker, &fixture, 11);

    assert!(
        chosen.iter().all(|choice| *choice == Some(1)),
        "a zero-weight member must never be drawn, got {chosen:?}"
    );
}

#[test]
fn weighted_random_answers_none_when_every_candidate_has_no_share() {
    let source = Arc::new(Scripted::new(vec![0]));
    let picker: Arc<dyn Picker> = Arc::new(WeightedRandom::new(source));
    let fixture = Fixture::named(&["a", "b"]).with_weights(&[0, 0]);

    assert_eq!(sequence(&picker, &fixture, 1), [None]);
}

#[test]
fn weighted_random_counts_only_the_candidates() {
    // Member 2 is not a candidate. Its share must not widen the range, or a draw would sometimes land
    // on a slot nobody occupies and the distribution would silently favour whoever follows it.
    let source = Arc::new(Scripted::new(vec![0, 1, 2]));
    let picker: Arc<dyn Picker> = Arc::new(WeightedRandom::new(source));
    let fixture = Fixture::named(&["a", "b", "c"]).with_weights(&[1, 2, 5]);
    let candidates = [0usize, 1];
    let view = View::new(&fixture, &candidates, None);

    let chosen: Vec<Option<usize>> = (0..3).map(|_| picker.pick(&view)).collect();

    assert_eq!(chosen, [Some(0), Some(1), Some(1)]);
}

#[test]
fn weighted_random_answers_none_without_candidates() {
    let source = Arc::new(Scripted::new(vec![0]));
    let picker: Arc<dyn Picker> = Arc::new(WeightedRandom::new(source));
    let fixture = Fixture::named(&["a"]);

    assert_eq!(picker.pick(&View::new(&fixture, &[], None)), None);
}

#[test]
fn weighted_random_keeps_its_source_across_a_publication() {
    // A draw holds no history, so nothing is carried but the source itself: the next sample must come
    // from the same sequence, not from a fresh one.
    let source = Arc::new(Scripted::new(vec![0, 1]));
    let picker: Arc<dyn Picker> = Arc::new(WeightedRandom::new(source));
    let fixture = Fixture::named(&["a", "b"]).with_weights(&[1, 1]);
    let _ = sequence(&picker, &fixture, 1);

    let names: Vec<Arc<str>> = fixture.names.clone();
    let inherited = picker.inherit(&names, &names);

    assert_eq!(sequence(&inherited, &fixture, 1), [Some(1)]);
}
