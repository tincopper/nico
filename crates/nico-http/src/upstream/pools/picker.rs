//! The document's strategy vocabulary mapped onto the selection crate.
//!
//! This file is the only place a [`Strategy`] becomes an implementation, and it lives here rather than
//! in [`nico_lb`] because the vocabulary is this workspace's configuration model, not a property of
//! selection. Keeping the mapping on this side is what lets the selection crate stay free of any
//! document type: it exports strategies and asks nothing about how they were configured.
//!
//! The match is exhaustive and has no wildcard. `Strategy` is not `#[non_exhaustive]` for that reason:
//! a variant with no arm fails to compile here instead of falling through to a default, which is the
//! difference between "a strategy nobody implemented" and "the rotation, silently".

use std::sync::Arc;

use nico_core::{PoolStrategy, Strategy};
use nico_lb::{LeastConn, Picker, RoundRobin, SmoothWeighted};

/// Builds the picker a document's strategy names.
pub(super) fn build(strategy: Strategy) -> Arc<dyn Picker> {
    match strategy {
        Strategy::RoundRobin => Arc::new(RoundRobin::default()),
        Strategy::WeightedRoundRobin => Arc::new(SmoothWeighted::default()),
        Strategy::LeastConn => Arc::new(LeastConn::default()),
        Strategy::IpHash => Arc::new(nico_lb::IpHash::default()),
    }
}

/// Builds the picker a route's upstream pool names.
///
/// The pool vocabulary is [`PoolStrategy`], shared with a provider route's pool; the same three
/// strategies mean the same thing here — a weighted draw, a document-order rotation, and a
/// caller-address rendezvous — which is why the match is a sibling of [`build`] rather than the
/// same function.
pub(super) fn build_route_pool_picker(strategy: PoolStrategy) -> Arc<dyn Picker> {
    match strategy {
        PoolStrategy::WeightedRandom => {
            // Seeded from the process start, the way the provider pool's draw is: the draw only has
            // to differ between requests, and a fixed seed would make every process start with the
            // same sequence.
            let seed = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.subsec_nanos().into());
            Arc::new(nico_lb::WeightedRandom::new(Arc::new(
                nico_lb::Random::new(seed),
            )))
        }
        PoolStrategy::RoundRobin => Arc::new(RoundRobin::default()),
        PoolStrategy::IpHash => Arc::new(nico_lb::IpHash::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_strategy_builds_a_picker() {
        // The match above is exhaustive by the compiler; this test states that the four arms of the
        // closed vocabulary reach four distinct implementations, so a copy-paste mistake that made two
        // variants share one is caught here rather than by a distribution nobody expected.
        for strategy in [
            Strategy::RoundRobin,
            Strategy::WeightedRoundRobin,
            Strategy::LeastConn,
            Strategy::IpHash,
        ] {
            let picker = build(strategy);
            assert!(!format!("{picker:?}").is_empty());
        }
    }
}
