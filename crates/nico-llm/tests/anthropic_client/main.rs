//! The Anthropic Messages client codec: one target, split by document.
//!
//! The codec is the client half of a route, so these tests never name a provider: they decode a wire
//! document into the canonical request and encode canonical values back into the documents a caller
//! of that protocol parses.

#![expect(
    clippy::expect_used,
    reason = "a test that cannot build its fixture has nothing useful to return"
)]

mod error;
mod request;
mod response;
mod stream;
mod support;
