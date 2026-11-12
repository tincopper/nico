//! Where a random draw comes from.
//!
//! Injected rather than reached for globally, for one reason that decides the shape: a distribution
//! strategy whose sample sequence is fixed can be *asserted*. "Roughly a third each" is a statistic
//! that hides the implementation; "these five draws reach members 0, 1, 0, 2, 0" is the behaviour the
//! document configured.
//!
//! The trait is one method wide on purpose. A picker needs a number in a range and nothing else —
//! shuffling, seeding, reseeding and cryptographic quality are all properties of an implementation,
//! and none of them belongs in this contract.

use std::sync::atomic::{AtomicU64, Ordering};

/// A source of numbers drawn from a range.
pub trait Source: Send + Sync + std::fmt::Debug {
    /// Returns a value in `0..total`.
    ///
    /// `total` is always positive: a caller with nothing to draw from answers without calling.
    fn below(&self, total: u64) -> u64;
}

/// The source a running gateway uses: a per-process counter run through a fixed mixer.
///
/// Not a cryptographic generator and not pretending to be. What a load balancer needs from a draw is
/// that two requests do not receive the same value in the same order, so members are not visited in a
/// fixed cycle; what it does *not* need is unpredictability against an adversary, because an adversary
/// choosing its own member would need to control this counter, not the draw.
///
/// The multiplexer is SplitMix64's finaliser, chosen because it is specified in arithmetic this file
/// contains: no dependency, no version that changes the sequence, and a test can replay it.
#[derive(Debug, Default)]
pub struct Random {
    /// The counter every `below` call advances.
    counter: AtomicU64,
}

impl Random {
    /// Builds a source whose first draw is mixed from `seed`.
    pub fn new(seed: u64) -> Self {
        Self {
            counter: AtomicU64::new(seed),
        }
    }
}

impl Source for Random {
    fn below(&self, total: u64) -> u64 {
        // `fetch_add` rather than a read-then-write: two concurrent draws must not mix the same
        // counter value, which would make them agree on a member as if the source were not there.
        let mixed = mix(self.counter.fetch_add(1, Ordering::Relaxed));
        // Modulo bias is bounded by `total` here: the weight ceiling is 1000, so the largest total a
        // route can express is a few thousand, and the bias is on the order of `total / 2^64`. The
        // alternative (rejection sampling) would need a loop for a defect no operator can observe.
        mixed % total
    }
}

/// Mixes `value` with SplitMix64's finaliser.
fn mix(value: u64) -> u64 {
    let mut mixed = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    mixed ^ (mixed >> 31)
}

#[cfg(test)]
mod tests;
