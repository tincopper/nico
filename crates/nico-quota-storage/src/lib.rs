//! Shared budget-store adapters: the seam between `nico-http`'s `BudgetStore` contract and the
//! backends a multi-replica deployment selects.
//!
//! [`nico-http`](nico_http) owns the *domain* contract — what a check and a charge mean — and
//! knows no backend. This crate owns the *backend seam*: the one atomic operation each adapter
//! must perform on its storage, expressed in vocabulary every KV system can serve (a compare-free
//! transaction over one key, a TTL, a value that carries the window and the charges already
//! recorded). The etcd and Redis adapters translate their wire protocols onto that seam; neither
//! protocol type crosses it.
//!
//! # Layout
//!
//! - [`seam`] — the backend-facing contract: one window record, the two atomic operations, the
//!   errors a wire can produce. This is what an adapter implements and what the fake imitates.
//! - `fake` (feature `fake`) — a deterministic in-memory backend: the same atomicity the real
//!   backends must show, driven by injected time and injected failures. The contract suite runs
//!   against it, and an adapter's tests run their backend semantics through it.
//! - `etcd` (feature `etcd`) and `redis` (feature `redis`) — the wire adapters.
//!
//! # The one invariant that shapes the seam
//!
//! A shared window must be updated by **one backend-atomic operation**: read, expiry check,
//! reset, dedupe, add, TTL refresh — all inside the storage, never as client-side
//! read-modify-write. Every adapter therefore sends a script/transaction per call; the seam's
//! operation signature is the promise that the storage does the whole decision.

#![deny(missing_docs)]

pub mod seam;

#[cfg(feature = "fake")]
pub mod fake;

#[cfg(feature = "fake")]
pub mod replica;

#[cfg(feature = "etcd")]
pub mod etcd;

#[cfg(feature = "redis")]
pub mod redis;
