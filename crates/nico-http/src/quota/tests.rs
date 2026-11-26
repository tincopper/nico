//! The fixed-window store: opening, accumulating, expiring, and the bound on its size.

use std::time::Duration;

use super::*;

/// A policy of `limit` tokens per `window` seconds.
fn policy(limit: u64, window: u32) -> BudgetPolicy {
    BudgetPolicy {
        limit_tokens: limit,
        window_seconds: window,
    }
}

/// A 32-byte key standing in for one identity on one route.
fn key(seed: u8) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0] = seed;
    k
}

#[test]
fn the_first_charge_opens_the_window() {
    let store = InProcessBudgetStore::new(16);
    let now = Instant::now();

    store.charge(&key(1), 100, &policy(1000, 60), now);

    assert!(
        store.check(&key(1), &policy(1000, 60), now).is_ok(),
        "100 of 1000 tokens spent leaves room"
    );
}

#[test]
fn charges_accumulate_inside_the_window() {
    let store = InProcessBudgetStore::new(16);
    let now = Instant::now();
    let policy = policy(1000, 60);

    store.charge(&key(1), 600, &policy, now);
    store.charge(&key(1), 399, &policy, now + Duration::from_secs(1));

    assert!(
        store
            .check(&key(1), &policy, now + Duration::from_secs(2))
            .is_ok(),
        "999 spent: one request's answer is still unknown, so the window has room"
    );

    // The charge that crosses the line flips the verdict for whatever comes next.
    store.charge(&key(1), 1, &policy, now + Duration::from_secs(2));
    assert!(
        store
            .check(&key(1), &policy, now + Duration::from_secs(2))
            .is_err(),
        "1000 spent: the window is spent"
    );
}

#[test]
fn an_expired_window_resets_the_spend() {
    let store = InProcessBudgetStore::new(16);
    let now = Instant::now();
    let policy = policy(1000, 60);

    store.charge(&key(1), 1000, &policy, now);
    assert!(store.check(&key(1), &policy, now).is_err());

    // One second past the window: the spend belongs to a closed window.
    assert!(
        store
            .check(&key(1), &policy, now + Duration::from_secs(61))
            .is_ok(),
        "a new window starts empty"
    );
}

#[test]
fn the_window_resets_only_when_its_duration_has_passed() {
    let store = InProcessBudgetStore::new(16);
    let now = Instant::now();
    let policy = policy(1000, 60);

    store.charge(&key(1), 1000, &policy, now);
    assert!(
        store
            .check(&key(1), &policy, now + Duration::from_secs(59))
            .is_err(),
        "one second before the reset the window is still spent"
    );
}

#[test]
fn a_charge_over_the_limit_is_recorded_and_refuses_the_next() {
    // The post-hoc semantics: one request may push past the limit, because its usage was unknown
    // until the provider answered. What is enforced is the *next* request.
    let store = InProcessBudgetStore::new(16);
    let now = Instant::now();
    let policy = policy(500, 60);

    store.charge(&key(1), 800, &policy, now);

    assert!(store.check(&key(1), &policy, now).is_err());
    assert!(
        !store.check(&key(2), &policy, now).is_err(),
        "another identity's window is its own"
    );
}

#[test]
fn identities_are_isolated() {
    let store = InProcessBudgetStore::new(16);
    let now = Instant::now();
    let policy = policy(500, 60);

    store.charge(&key(1), 500, &policy, now);
    store.charge(&key(2), 10, &policy, now);

    assert!(store.check(&key(1), &policy, now).is_err());
    assert!(store.check(&key(2), &policy, now).is_ok());
}

#[test]
fn the_table_is_bounded_by_max_keys() {
    let store = InProcessBudgetStore::new(2);
    let now = Instant::now();
    let policy = policy(1000, 60);

    store.charge(&key(1), 1, &policy, now);
    store.charge(&key(2), 1, &policy, now);
    store.charge(&key(3), 1, &policy, now);
    store.charge(&key(4), 1, &policy, now);

    assert!(store.check(&key(1), &policy, now).is_ok());
    assert!(store.check(&key(4), &policy, now).is_ok());
}

#[test]
fn tenant_and_subject_quota_domains_are_distinct_and_route_scoped() {
    assert_eq!(
        budget_key("llm-chat", "tenant-a", true),
        budget_key("llm-chat", "tenant-a", true)
    );
    assert_ne!(
        budget_key("llm-chat", "tenant-a", true),
        budget_key("llm-chat", "tenant-a", false)
    );
    assert_ne!(
        budget_key("llm-chat", "tenant-a", true),
        budget_key("llm-embed", "tenant-a", true)
    );
}

#[test]
fn tenant_keys_share_the_same_route_window_across_principals() {
    let store = InProcessBudgetStore::new(16);
    let now = Instant::now();
    let policy = policy(500, 60);
    let first_key = budget_key("llm-chat", "tenant-acme", true);
    let second_key = budget_key("llm-chat", "tenant-acme", true);

    store.charge(&first_key, 500, &policy, now);

    assert!(store.check(&second_key, &policy, now).is_err());
}

#[test]
fn distinct_routes_charge_distinct_windows() {
    let store = InProcessBudgetStore::new(16);
    let now = Instant::now();
    let policy = policy(500, 60);
    let route_a = budget_key("llm-chat", "user-1", false);
    let route_b = budget_key("llm-embed", "user-1", false);

    store.charge(&route_a, 500, &policy, now);

    assert!(store.check(&route_a, &policy, now).is_err());
    assert!(store.check(&route_b, &policy, now).is_ok());
}
