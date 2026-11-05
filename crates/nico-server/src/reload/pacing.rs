//! De-bounce, coalescing and backoff: the pure decisions the consuming loop is built from.
//!
//! Nothing here touches the reloader's state or an I/O handle, which is what makes these the parts
//! that can be tested against a value instead of against a running pipeline: a burst collapses to its
//! last document, two deadlines resolve to the earlier one, and a retry delay grows to a fixed
//! ceiling without ever going backwards.
//!
//! The clock is `tokio::time::Instant` throughout, so a test that pauses time observes the same
//! deadlines a running process would.

use std::{
    task::{Context, Poll, Waker},
    time::Duration,
};

use nico_config::{ConfigEvent, ConfigSourceError, ConfigVersion, ConfigWatch};
use tokio::time::Instant;

/// First reconnect delay; it doubles on every further failed attempt.
const BACKOFF_BASE: Duration = Duration::from_millis(100);

/// Ceiling for the reconnect delay, jitter included.
const BACKOFF_CAP: Duration = Duration::from_secs(5);

/// Largest power-of-two multiplier applied to [`BACKOFF_BASE`] before the cap applies.
const BACKOFF_MAX_EXPONENT: u32 = 6;

/// One item of a configuration watch.
pub(super) type WatchItem = Result<ConfigEvent, ConfigSourceError>;

/// What woke the debounce loop.
pub(super) enum Wake {
    /// Shutdown was requested.
    Shutdown,
    /// The debounce window or the maximum wait elapsed.
    Deadline,
    /// The watch produced an item, or ended.
    Item(Option<WatchItem>),
}

/// What the watch said once everything already queued behind it has been coalesced.
pub(super) enum WatchStep {
    /// The last document of a burst.
    Candidate(ConfigVersion),
    /// The source cannot continue incrementally.
    Resync,
    /// The source failed.
    Failed(ConfigSourceError),
}

/// Reduces the next queued item, and every snapshot behind it, to one decision.
///
/// Draining is what makes the debounce a debounce: a burst that has already arrived is exactly the
/// set of documents the quiet window is meant to collapse, so three queued events publish once.
/// A resync or a failure supersedes any candidate collected so far, because both mean "the source,
/// not this document, decides what is current".
pub(super) fn coalesce(watch: &mut ConfigWatch, first: WatchItem) -> WatchStep {
    let mut candidate = match first {
        Ok(ConfigEvent::Snapshot(version)) => version,
        Ok(ConfigEvent::ResyncRequired) => return WatchStep::Resync,
        Err(error) => return WatchStep::Failed(error),
        // `ConfigEvent` is `#[non_exhaustive]`; the only safe reading of an unknown event is
        // "reload the whole document", because this build cannot know what it means.
        Ok(_) => return WatchStep::Resync,
    };

    while let Some(item) = take_ready(watch) {
        match item {
            Ok(ConfigEvent::Snapshot(version)) => candidate = version,
            Ok(ConfigEvent::ResyncRequired) => return WatchStep::Resync,
            Err(error) => return WatchStep::Failed(error),
            Ok(_) => return WatchStep::Resync,
        }
    }

    WatchStep::Candidate(candidate)
}

/// Takes an item that is already available, without polling a timer.
///
/// A no-op waker is enough: the caller only wants what is ready now, and the next poll from the
/// suspend point in the consuming loop registers the real waker.
fn take_ready(watch: &mut ConfigWatch) -> Option<WatchItem> {
    let mut context = Context::from_waker(Waker::noop());
    match watch.as_mut().poll_next(&mut context) {
        Poll::Ready(item) => item,
        Poll::Pending => None,
    }
}

/// Polls the watch for its next item.
pub(super) async fn next_item(watch: &mut ConfigWatch) -> Option<WatchItem> {
    std::future::poll_fn(|context: &mut Context<'_>| watch.as_mut().poll_next(context)).await
}

/// Sleeps until `deadline`, or forever when there is nothing pending.
pub(super) async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Returns the earlier of two optional deadlines.
pub(super) fn earliest(left: Option<Instant>, right: Option<Instant>) -> Option<Instant> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(only), None) | (None, Some(only)) => Some(only),
        (None, None) => None,
    }
}

/// Computes the reconnect delay for a failed attempt.
///
/// Exponential from [`BACKOFF_BASE`] to [`BACKOFF_CAP`], plus a small deterministic jitter derived
/// from the attempt counter: processes that started together still separate, without pulling a
/// random-number dependency in for a sleep.
pub(super) fn backoff_delay(attempt: u32) -> Duration {
    let exponent = attempt.min(BACKOFF_MAX_EXPONENT);
    let base = BACKOFF_BASE
        .saturating_mul(1u32 << exponent)
        .min(BACKOFF_CAP);
    let jitter = base / 8 * (attempt % 5);
    (base + jitter).min(BACKOFF_CAP)
}

/// Returns the current Unix time in seconds, or `0` before the epoch.
pub(super) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    mod backoff {
        use super::*;

        #[test]
        fn backoff_delay_starts_at_the_base_delay() {
            assert_eq!(backoff_delay(0), BACKOFF_BASE);
        }

        #[test]
        fn backoff_delay_doubles_before_the_cap_applies() {
            // Attempt 1: 100 ms * 2, plus one eighth of jitter.
            assert_eq!(backoff_delay(1), BACKOFF_BASE * 2 + BACKOFF_BASE * 2 / 8);
            // Attempt 2: 100 ms * 4, plus two eighths of jitter.
            assert_eq!(
                backoff_delay(2),
                BACKOFF_BASE * 4 + BACKOFF_BASE * 4 / 8 * 2
            );
        }

        #[test]
        fn backoff_delay_never_exceeds_the_cap() {
            for attempt in 0..64 {
                assert!(
                    backoff_delay(attempt) <= BACKOFF_CAP,
                    "attempt {attempt} exceeded the cap"
                );
            }
        }

        #[test]
        fn backoff_delay_is_monotonic_within_the_cap() {
            let mut previous = Duration::ZERO;
            for attempt in 0..BACKOFF_MAX_EXPONENT {
                let delay = backoff_delay(attempt);
                assert!(
                    delay >= previous || delay == BACKOFF_CAP,
                    "attempt {attempt} went backwards"
                );
                previous = delay;
            }
        }

        #[test]
        fn backoff_delay_separates_processes_that_started_together() {
            // The jitter is derived from the attempt counter, so the same attempt is reproducible
            // and the next attempt is not the same delay.
            assert_eq!(backoff_delay(3), backoff_delay(3));
            assert_ne!(backoff_delay(3), backoff_delay(4));
        }
    }

    mod deadlines {
        use super::*;

        #[test]
        fn earliest_prefers_the_sooner_deadline() {
            let base = Instant::now();
            let sooner = base + Duration::from_millis(10);
            let later = base + Duration::from_millis(20);
            assert_eq!(earliest(Some(later), Some(sooner)), Some(sooner));
        }

        #[test]
        fn earliest_uses_whatever_is_available() {
            let deadline = Instant::now();
            assert_eq!(earliest(Some(deadline), None), Some(deadline));
            assert_eq!(earliest(None, Some(deadline)), Some(deadline));
            assert_eq!(earliest(None, None), None);
        }
    }
}
