//! Tests for the bounded bucket table: the arithmetic of a token bucket and the bound on its memory.
//!
//! The clock is Tokio's paused clock (`start_paused`), so a refill is driven by an explicit `advance`
//! instead of by sleeping: a test that waited for a real second would be slower and flakier without
//! asserting anything more.

use std::time::Duration;

use tokio::time::{Instant, advance};

use super::{LimitParameters, Limiter};

/// One bucket key.
fn key(tag: u8) -> [u8; 32] {
    [tag; 32]
}

/// Builds a limiter with `burst`, `per_second` and `max_keys`.
fn limiter(burst: u32, per_second: u32, max_keys: u32) -> Limiter {
    let parameters = LimitParameters::new("quota", burst, per_second, max_keys)
        .expect("the fixture parameters are valid");
    Limiter::new(parameters)
}

#[tokio::test(start_paused = true)]
async fn a_full_bucket_serves_its_burst_and_then_refuses() {
    let limiter = limiter(3, 1, 16);

    for request in 1..=3 {
        assert!(
            limiter.acquire(key(1), Instant::now()).is_ok(),
            "request {request} of the burst must be served"
        );
    }
    assert!(
        limiter.acquire(key(1), Instant::now()).is_err(),
        "the fourth request has no token"
    );
}

#[tokio::test(start_paused = true)]
async fn a_refill_restores_one_token_per_second() {
    let limiter = limiter(1, 1, 16);
    assert!(limiter.acquire(key(1), Instant::now()).is_ok());

    advance(Duration::from_millis(500)).await;
    assert!(
        limiter.acquire(key(1), Instant::now()).is_err(),
        "half a second is not a whole token"
    );

    advance(Duration::from_millis(500)).await;
    assert!(
        limiter.acquire(key(1), Instant::now()).is_ok(),
        "a second at one token per second is one token"
    );
}

#[tokio::test(start_paused = true)]
async fn a_refused_request_is_told_when_a_token_returns() {
    let limiter = limiter(1, 4, 16);
    assert!(limiter.acquire(key(1), Instant::now()).is_ok());

    let wait = limiter
        .acquire(key(1), Instant::now())
        .expect_err("the bucket is empty");
    assert_eq!(
        wait,
        Duration::from_millis(250),
        "a quarter of a second restores one token at four per second"
    );

    advance(wait).await;
    assert!(
        limiter.acquire(key(1), Instant::now()).is_ok(),
        "waiting exactly as long as told must be enough"
    );
}

#[tokio::test(start_paused = true)]
async fn idle_time_never_accumulates_more_than_the_burst() {
    let limiter = limiter(2, 1, 16);
    assert!(limiter.acquire(key(1), Instant::now()).is_ok());
    assert!(limiter.acquire(key(1), Instant::now()).is_ok());

    // An idle minute is sixty tokens' worth of refill, but a bucket holds two.
    advance(Duration::from_secs(60)).await;
    assert!(limiter.acquire(key(1), Instant::now()).is_ok());
    assert!(limiter.acquire(key(1), Instant::now()).is_ok());
    assert!(
        limiter.acquire(key(1), Instant::now()).is_err(),
        "a burst is a ceiling, not a rate the idle time may raise"
    );
}

#[tokio::test(start_paused = true)]
async fn each_key_gets_its_own_bucket() {
    let limiter = limiter(1, 1, 16);
    assert!(limiter.acquire(key(1), Instant::now()).is_ok());
    assert!(limiter.acquire(key(1), Instant::now()).is_err());
    assert!(
        limiter.acquire(key(2), Instant::now()).is_ok(),
        "one caller spending its token must not charge another"
    );
}

#[tokio::test(start_paused = true)]
async fn the_table_evicts_the_least_recently_used_bucket() {
    let limiter = limiter(1, 1, 2);
    assert!(limiter.acquire(key(1), Instant::now()).is_ok());
    assert!(limiter.acquire(key(2), Instant::now()).is_ok());
    assert_eq!(limiter.len(), 2);

    // A third key needs room: the oldest bucket goes, because a caller changing its key must not be
    // able to grow the table.
    assert!(limiter.acquire(key(3), Instant::now()).is_ok());
    assert_eq!(limiter.len(), 2);

    // The evicted key starts over, so it is served again before its refill would have allowed it.
    assert!(
        limiter.acquire(key(1), Instant::now()).is_ok(),
        "an evicted bucket is a new bucket, which is the documented cost of a bounded table"
    );
}

#[tokio::test(start_paused = true)]
async fn the_table_never_holds_more_than_its_bound() {
    let limiter = limiter(1, 1, 4);

    for tag in 0..64 {
        assert!(limiter.acquire(key(tag), Instant::now()).is_ok());
        assert!(
            limiter.len() <= 4,
            "the table held {} buckets with a bound of 4",
            limiter.len()
        );
    }
}

#[test]
fn the_parameters_are_readable_so_a_compiler_can_decide_whether_to_reuse_the_table() {
    let parameters = LimitParameters::new("quota", 10, 5, 64).expect("the fixture is valid");
    let limiter = Limiter::new(parameters);

    assert_eq!(limiter.parameters(), parameters);
    assert_eq!(limiter.parameters().burst().get(), 10);
    assert_eq!(limiter.parameters().per_second().get(), 5);
    assert_eq!(limiter.parameters().max_keys().get(), 64);
}
