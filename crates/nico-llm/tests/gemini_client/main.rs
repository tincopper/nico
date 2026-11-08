//! The Gemini client codec: one target, split by document.

#![expect(
    clippy::expect_used,
    reason = "a test that cannot build its fixture has nothing useful to return"
)]

mod error;
mod parts;
mod request;
mod response;
mod stream;
mod support;
mod target;
