//! The chain's own rules: order, short-circuiting, body rewriting and the budget.
//!
//! The subject is the driver, not the plugins: which stage runs when, what stops the chain, and what a
//! stage may do to the bytes. `scope` order (global before route, route before global for the response)
//! is decided by the hooks and asserted in `proxy/tests.rs` and the integration targets.
//!
//! Shared fakes live in `fixture`; the assertions are split by subject — `control` for order,
//! short-circuiting, the `Respond` boundary and attribution, `bytes` for body rewriting, the budget and
//! framing declarations.

mod bytes;
mod control;
mod fixture;
