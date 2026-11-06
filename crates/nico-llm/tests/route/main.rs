//! The front-ends the data plane publishes: a converting route and a passthrough route.
//!
//! `convert` and `passthrough` are separate files because they are separate contracts: one decodes
//! and re-encodes a body, the other must not touch it at all. `transform` covers the response half of
//! the converting route, which is where a stream's ordering and a buffered ceiling are decided.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a test that cannot build its fixture has nothing useful to return"
)]

mod convert;
mod passthrough;
mod support;
mod transform;
