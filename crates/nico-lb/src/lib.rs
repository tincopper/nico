//! Member selection: *which* one of several interchangeable members serves this request.
//!
//! The crate answers one question and nothing else. It knows a member by four facts — its name, its
//! weight, how many requests are using it, and its position in a candidate list — and it never learns
//! what a member *is*: not a socket address, not a provider, not a URL. That is what makes the same
//! four strategies serve two different pools (upstream addresses and model providers) without either
//! domain's vocabulary appearing here.
//!
//! The shape has three parts, and each removes one way a selection layer drifts:
//!
//! - **[`Picker`] owns its state.** A rotation position, a smooth-weight vector, a tie-break counter:
//!   each lives in the implementation that reads it. A caller therefore holds an `Arc<dyn Picker>` and
//!   knows no strategy's layout, and a strategy that needs no state costs neither an allocation nor a
//!   lock for the strategies that do.
//! - **[`View`] is one input, not one signature per strategy.** A rule reads the accessors it needs
//!   and nothing else, so adding an input is a new accessor rather than a new argument on every
//!   implementation. [`Members`] is the same seam from the other side: it is what a rule may know
//!   about a member, and any pool implements it without the rules changing.
//! - **[`Picker::inherit`] carries state forward by identity, not by index.** A caller hands the two
//!   generations' member names, so a member that kept its identity keeps what it accumulated while a
//!   member that changed is a different member and starts from nothing.
//!
//! # What this crate deliberately does not do
//!
//! It does not decide *availability*. A caller filters the candidate list before building a [`View`],
//! and the contract is that a picker returns one of the indices it was handed — never a member it was
//! not offered. Health, ejection, cooldowns and capability filtering are the pool's business; this
//! crate only orders what it is given, which is why it needs no clock, no configuration and no I/O.
//!
//! It also does not map a configuration enum onto an implementation. That mapping is an exhaustive
//! `match` in the crate that owns the document vocabulary, so a new strategy fails to compile where
//! the vocabulary lives rather than being silently defaulted here.
//!
//! # Strategies
//!
//! | Type | Chooses | State |
//! |---|---|---|
//! | [`RoundRobin`] | the candidates in order | a position |
//! | [`SmoothWeighted`] | in proportion to weight, interleaved | every member's running weight |
//! | [`LeastConn`] | the member with fewest requests in flight | a tie-break position |
//! | [`IpHash`] | the member the key hashes onto | none |
//!
//! Two of them have a property worth stating because the wrong implementation still looks plausible:
//! [`SmoothWeighted`] is nginx's smooth weighted round robin and not "draw a member with probability
//! `weight / total`" (which bursts and cannot be asserted in a test), and [`IpHash`] is a rendezvous
//! hash and not `hash % len` (adding a member moves about `1/n` of the keys instead of nearly all).

#![deny(missing_docs)]

mod picker;
mod source;
mod weighted;

pub use picker::{LeastConn, Members, Picker, RoundRobin, SmoothWeighted, View};
pub use source::{Random, Source};
pub use weighted::{IpHash, WeightedRandom};
