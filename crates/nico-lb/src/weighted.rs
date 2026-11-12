//! The two strategies that are about *distribution* rather than *order*: one keyed on the caller, one
//! drawn at random from the weights.

use std::{
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use crate::{
    picker::{Picker, View},
    source::Source,
};

/// Returns the candidate the caller's address hashes onto.
///
/// The same caller keeps landing on the same member while the member set is unchanged, which is what a
/// stateful or cache-warming pool needs. The key is the caller's address and never a header, so it
/// cannot be chosen by the caller.
#[derive(Debug, Default)]
pub struct IpHash {
    /// The rotation used when a request has no address to key on.
    ///
    /// A caller with no peer address (a Unix socket, a session a test built itself) has nothing to be
    /// sticky on. Inventing a key would pile every such caller onto one member, so the rotation
    /// spreads them instead; it lives here because it is this rule's own fallback, not the pool's.
    address_less: AtomicUsize,
}

impl Picker for IpHash {
    fn pick(&self, view: &View<'_>) -> Option<usize> {
        let candidates = view.candidates();
        if candidates.is_empty() {
            return None;
        }
        let Some(address) = view.client() else {
            let slot = self.address_less.fetch_add(1, Ordering::Relaxed);
            return candidates.get(slot % candidates.len()).copied();
        };
        let key = key_bytes(address);
        let mut best: Option<(usize, u64)> = None;
        for &index in candidates {
            let candidate = score(&key, view.name(index));
            if best.is_none_or(|(_, best_score)| candidate > best_score) {
                best = Some((index, candidate));
            }
        }
        best.map(|(index, _)| index)
    }

    fn inherit(&self, _previous: &[Arc<str>], _current: &[Arc<str>]) -> Arc<dyn Picker> {
        // The mapping is a function of the key and the member names, both of which the next generation
        // carries; there is nothing to keep.
        Arc::new(Self::default())
    }
}

/// Draws a candidate in proportion to its weight.
///
/// This is the distribution a provider pool defaults to, and it differs from [`SmoothWeighted`] in
/// what it promises: the smooth rotation promises an *order* (a `3:1` pair interleaves), while this
/// promises only a *proportion over samples*. A caller that needs the first cannot use this one.
///
/// **Zero-weight members are not candidates here by construction**, not by a skip: they contribute no
/// width to the range the draw lands in, so a member with no share cannot be drawn even if the
/// arithmetic were to go wrong elsewhere.
///
/// [`SmoothWeighted`]: crate::SmoothWeighted
pub struct WeightedRandom {
    /// The draw's source of randomness.
    ///
    /// Injected rather than global so a test can state the sequence it expects, and so the crate needs
    /// no random-number dependency of its own.
    source: Arc<dyn Source>,
}

impl WeightedRandom {
    /// Builds the strategy over the given randomness source.
    pub fn new(source: Arc<dyn Source>) -> Self {
        Self { source }
    }
}

impl std::fmt::Debug for WeightedRandom {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WeightedRandom")
            .finish_non_exhaustive()
    }
}

impl Picker for WeightedRandom {
    fn pick(&self, view: &View<'_>) -> Option<usize> {
        let candidates = view.candidates();
        if candidates.is_empty() {
            return None;
        }
        // The total is the candidates' weights, not every member's: an unavailable member must not
        // widen the range, or the draw would sometimes land on a slot nobody occupies.
        let total: u64 = candidates
            .iter()
            .map(|&index| u64::from(view.weight(index)))
            .sum();
        if total == 0 {
            // Every candidate has no share. Drawing from an empty range has no meaning, and refusing
            // to choose is the honest answer; a caller that reaches here has a document where every
            // member is a fallback-only member.
            return None;
        }
        let mut point = self.source.below(total);
        for &index in candidates {
            let share = u64::from(view.weight(index));
            if share == 0 {
                continue;
            }
            if point < share {
                return Some(index);
            }
            point -= share;
        }
        // Unreachable while `below` honours its contract (`< total`). Answering with the last weighted
        // candidate rather than panicking keeps a source that breaks the contract from taking the
        // process down; the sequence would be wrong, and a test with a deterministic source states the
        // right one.
        candidates
            .iter()
            .rev()
            .copied()
            .find(|&index| view.weight(index) > 0)
    }

    fn inherit(&self, _previous: &[Arc<str>], _current: &[Arc<str>]) -> Arc<dyn Picker> {
        // A draw holds no history: the next sample is independent of the last, which is the whole
        // difference between this and a rotation.
        Arc::new(Self {
            source: Arc::clone(&self.source),
        })
    }
}

/// Returns the bytes [`IpHash`] hashes: the caller's address in network order.
///
/// The address and nothing else — not the `Host` header, not the path. A key a caller can choose would
/// let it pick its own member, and a key the caller's ISP shares (a NAT address) is what makes one
/// office land on one machine, which is the point of the strategy.
fn key_bytes(address: IpAddr) -> Vec<u8> {
    match address {
        IpAddr::V4(address) => address.octets().to_vec(),
        IpAddr::V6(address) => address.octets().to_vec(),
    }
}

/// Scores one `(key, member name)` pair with FNV-1a.
///
/// FNV-1a rather than a hasher from `std`: the score must not change when the toolchain does, because
/// it decides which machine serves a caller. FNV-1a is a fixed, specified function over bytes, so the
/// mapping is a property of the configuration rather than of the build that read it.
fn score(key: &[u8], member: &str) -> u64 {
    /// The FNV-1a 64-bit offset basis.
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    /// The FNV-1a 64-bit prime.
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET_BASIS;
    for &byte in key.iter().chain(b"\x1f".iter()).chain(member.as_bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests;
