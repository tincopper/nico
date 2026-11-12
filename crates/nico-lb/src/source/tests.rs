//! The default source: it must not repeat a value in a fixed cycle, and it must stay in range.

use super::*;

#[test]
fn the_source_never_leaves_the_range() {
    let source = Random::new(7);
    for total in [1u64, 2, 3, 1000] {
        for _ in 0..64 {
            assert!(source.below(total) < total);
        }
    }
}

#[test]
fn consecutive_draws_do_not_repeat_the_counter() {
    // What a load balancer needs from a draw: two requests must not receive the same value in the
    // same order, or the members are visited in a fixed cycle that no weight asked for.
    let source = Random::new(0);
    let draws: Vec<u64> = (0..8).map(|_| source.below(1000)).collect();

    assert!(
        draws.windows(2).any(|pair| pair[0] != pair[1]),
        "eight draws over a range of a thousand should not be constant, got {draws:?}"
    );
}

#[test]
fn a_seeded_source_replays_its_sequence() {
    // The mixer is specified arithmetic, so the same seed is the same sequence: a test can rely on it
    // and a deployment cannot be surprised by a toolchain upgrade.
    let first: Vec<u64> = {
        let source = Random::new(42);
        (0..5).map(|_| source.below(1000)).collect()
    };
    let second: Vec<u64> = {
        let source = Random::new(42);
        (0..5).map(|_| source.below(1000)).collect()
    };

    assert_eq!(first, second);
}
