//! Tests for the parts of the writer that decide a value rather than write it.
//!
//! The header set itself is asserted end to end (`nico-server/tests/plugins_*`): a `Session` needs a
//! connection, so a unit test can only reach the arithmetic and the mapping that feed it.

use std::time::Duration;

use super::retry_after_seconds;

#[test]
fn a_wait_rounds_up_to_whole_seconds() {
    assert_eq!(retry_after_seconds(Duration::from_millis(1)), 1);
    assert_eq!(retry_after_seconds(Duration::from_millis(900)), 1);
    assert_eq!(retry_after_seconds(Duration::from_millis(1_000)), 1);
    assert_eq!(retry_after_seconds(Duration::from_millis(1_500)), 2);
    assert_eq!(retry_after_seconds(Duration::from_secs(7)), 7);
}

#[test]
fn a_zero_wait_stays_zero() {
    assert_eq!(
        retry_after_seconds(Duration::ZERO),
        0,
        "a bucket that has a token available needs no wait, and `0` says exactly that"
    );
}
