//! The fixed-window store: opening, accumulating, expiring, and the bound on its size.

use std::time::Duration;

use super::{local::InProcessBudgetStore, *};

/// A policy of `limit` tokens per `window` seconds.
fn policy(limit: u64, window: u32) -> BudgetPolicy {
    BudgetPolicy {
        limit_tokens: limit,
        window_seconds: window,
    }
}

/// A 32-byte key standing in for one identity on one route.
fn key(seed: u8) -> BudgetKey {
    let mut k = [0u8; 32];
    k[0] = seed;
    k
}

/// A charge of `tokens` at `offset` seconds after `now`, under one stable id.
fn make_charge(now: SystemTime, id: &str, tokens: u64, offset: u64) -> Charge {
    Charge {
        id: ChargeId(id.into()),
        input: tokens,
        output: 0,
        at: now + Duration::from_secs(offset),
    }
}

async fn verdict(
    store: &InProcessBudgetStore,
    key: &BudgetKey,
    policy: &BudgetPolicy,
    now: SystemTime,
) -> Result<CheckResult, BudgetStoreError> {
    BudgetStore::check(store, key, policy, now).await
}

async fn record(
    store: &InProcessBudgetStore,
    key: &BudgetKey,
    policy: &BudgetPolicy,
    charge: &Charge,
) -> Result<(), BudgetStoreError> {
    BudgetStore::charge(store, key, policy, charge).await
}

#[tokio::test]
async fn the_first_charge_opens_the_window() {
    let store = InProcessBudgetStore::new(16);
    let now = SystemTime::UNIX_EPOCH;

    record(
        &store,
        &key(1),
        &policy(1000, 60),
        &make_charge(now, "c1", 100, 0),
    )
    .await
    .expect("the first charge lands");

    assert!(
        matches!(
            verdict(&store, &key(1), &policy(1000, 60), now).await,
            Ok(CheckResult::Allowed)
        ),
        "100 of 1000 tokens spent leaves room"
    );
}

#[tokio::test]
async fn charges_accumulate_inside_the_window() {
    let store = InProcessBudgetStore::new(16);
    let now = SystemTime::UNIX_EPOCH;
    let policy = policy(1000, 60);

    record(&store, &key(1), &policy, &make_charge(now, "c1", 600, 0))
        .await
        .expect("the first charge lands");
    record(&store, &key(1), &policy, &make_charge(now, "c2", 399, 1))
        .await
        .expect("the second charge lands");

    assert!(
        matches!(
            verdict(&store, &key(1), &policy, now + Duration::from_secs(2)).await,
            Ok(CheckResult::Allowed)
        ),
        "999 spent: one request's answer is still unknown, so the window has room"
    );

    // The charge that crosses the line flips the verdict for whatever comes next.
    record(&store, &key(1), &policy, &make_charge(now, "c3", 1, 2))
        .await
        .expect("the crossing charge lands");
    assert!(
        matches!(
            verdict(&store, &key(1), &policy, now + Duration::from_secs(2)).await,
            Ok(CheckResult::Exceeded(_))
        ),
        "1000 spent: the window is spent"
    );
}

#[tokio::test]
async fn an_expired_window_resets_the_spend() {
    let store = InProcessBudgetStore::new(16);
    let now = SystemTime::UNIX_EPOCH;
    let policy = policy(1000, 60);

    record(&store, &key(1), &policy, &make_charge(now, "c1", 1000, 0))
        .await
        .expect("the charge lands");
    assert!(matches!(
        verdict(&store, &key(1), &policy, now).await,
        Ok(CheckResult::Exceeded(_))
    ));

    // One second past the window: the spend belongs to a closed window.
    assert!(
        matches!(
            verdict(&store, &key(1), &policy, now + Duration::from_secs(61)).await,
            Ok(CheckResult::Allowed)
        ),
        "a new window starts empty"
    );
}

#[tokio::test]
async fn the_window_resets_only_when_its_duration_has_passed() {
    let store = InProcessBudgetStore::new(16);
    let now = SystemTime::UNIX_EPOCH;
    let policy = policy(1000, 60);

    record(&store, &key(1), &policy, &make_charge(now, "c1", 1000, 0))
        .await
        .expect("the charge lands");
    assert!(matches!(
        verdict(&store, &key(1), &policy, now + Duration::from_secs(59)).await,
        Ok(CheckResult::Exceeded(_))
    ));
}

#[tokio::test]
async fn a_charge_over_the_limit_is_recorded_and_refuses_the_next() {
    // The post-hoc semantics: one request may push past the limit, because its usage was unknown
    // until the provider answered. What is enforced is the *next* request.
    let store = InProcessBudgetStore::new(16);
    let now = SystemTime::UNIX_EPOCH;
    let policy = policy(500, 60);

    record(&store, &key(1), &policy, &make_charge(now, "c1", 800, 0))
        .await
        .expect("the over-limit charge is still recorded");

    assert!(matches!(
        verdict(&store, &key(1), &policy, now).await,
        Ok(CheckResult::Exceeded(_))
    ));
    assert!(
        matches!(
            verdict(&store, &key(2), &policy, now).await,
            Ok(CheckResult::Allowed)
        ),
        "another identity's window is its own"
    );
}

#[tokio::test]
async fn a_retried_charge_with_the_same_id_bills_once() {
    let store = InProcessBudgetStore::new(16);
    let now = SystemTime::UNIX_EPOCH;
    let policy = policy(1000, 60);

    record(&store, &key(1), &policy, &make_charge(now, "same", 300, 0))
        .await
        .expect("the first write lands");
    record(&store, &key(1), &policy, &make_charge(now, "same", 300, 0))
        .await
        .expect("the retried write is accepted");

    assert!(
        matches!(
            verdict(&store, &key(1), &policy, now).await,
            Ok(CheckResult::Allowed)
        ),
        "the retried id must not bill twice: 300 spent, not 600"
    );
}

#[tokio::test]
async fn identities_are_isolated() {
    let store = InProcessBudgetStore::new(16);
    let now = SystemTime::UNIX_EPOCH;
    let policy = policy(500, 60);

    record(&store, &key(1), &policy, &make_charge(now, "c1", 500, 0))
        .await
        .expect("the charge lands");
    record(&store, &key(2), &policy, &make_charge(now, "c2", 10, 0))
        .await
        .expect("the charge lands");

    assert!(matches!(
        verdict(&store, &key(1), &policy, now).await,
        Ok(CheckResult::Exceeded(_))
    ));
    assert!(matches!(
        verdict(&store, &key(2), &policy, now).await,
        Ok(CheckResult::Allowed)
    ));
}

#[tokio::test]
async fn the_table_is_bounded_by_max_keys() {
    let store = InProcessBudgetStore::new(2);
    let now = SystemTime::UNIX_EPOCH;
    let policy = policy(1000, 60);

    for seed in 1..=4 {
        record(&store, &key(seed), &policy, &make_charge(now, "c", 1, 0))
            .await
            .expect("the charge lands");
    }

    assert!(matches!(
        verdict(&store, &key(1), &policy, now).await,
        Ok(CheckResult::Allowed)
    ));
    assert!(matches!(
        verdict(&store, &key(4), &policy, now).await,
        Ok(CheckResult::Allowed)
    ));
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

#[tokio::test]
async fn tenant_keys_share_the_same_route_window_across_principals() {
    let store = InProcessBudgetStore::new(16);
    let now = SystemTime::UNIX_EPOCH;
    let policy = policy(500, 60);
    let first_key = budget_key("llm-chat", "tenant-acme", true);
    let second_key = budget_key("llm-chat", "tenant-acme", true);

    record(&store, &first_key, &policy, &make_charge(now, "c1", 500, 0))
        .await
        .expect("the charge lands");

    assert!(matches!(
        verdict(&store, &second_key, &policy, now).await,
        Ok(CheckResult::Exceeded(_))
    ));
}

#[tokio::test]
async fn distinct_routes_charge_distinct_windows() {
    let store = InProcessBudgetStore::new(16);
    let now = SystemTime::UNIX_EPOCH;
    let policy = policy(500, 60);
    let route_a = budget_key("llm-chat", "user-1", false);
    let route_b = budget_key("llm-embed", "user-1", false);

    record(&store, &route_a, &policy, &make_charge(now, "c1", 500, 0))
        .await
        .expect("the charge lands");

    assert!(matches!(
        verdict(&store, &route_a, &policy, now).await,
        Ok(CheckResult::Exceeded(_))
    ));
    assert!(matches!(
        verdict(&store, &route_b, &policy, now).await,
        Ok(CheckResult::Allowed)
    ));
}

/// A store whose every operation fails with the given error: the injected-outage subject the
/// policy tests need.
struct FailingStore;

impl std::fmt::Debug for FailingStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FailingStore")
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl BudgetStore for FailingStore {
    async fn check(
        &self,
        _key: &BudgetKey,
        _policy: &BudgetPolicy,
        _now: SystemTime,
    ) -> Result<CheckResult, BudgetStoreError> {
        Err(BudgetStoreError::Unavailable {
            source: "the wire broke".into(),
        })
    }

    async fn charge(
        &self,
        _key: &BudgetKey,
        _policy: &BudgetPolicy,
        _charge: &Charge,
    ) -> Result<(), BudgetStoreError> {
        Err(BudgetStoreError::Unavailable {
            source: "the wire broke".into(),
        })
    }
}

#[tokio::test]
async fn a_failing_store_reports_unavailable_not_exceeded() {
    let store = FailingStore;
    let now = SystemTime::UNIX_EPOCH;

    let error = BudgetStore::check(&store, &key(1), &policy(10, 60), now)
        .await
        .expect_err("the injected outage is returned");
    assert!(
        matches!(error, BudgetStoreError::Unavailable { .. }),
        "the store's failure is its own fact, never a spent window"
    );
}

#[test]
fn the_policy_names_match_the_bootstrap_values() {
    assert_eq!(UnavailablePolicy::FailClosed.name(), "fail-closed");
    assert_eq!(UnavailablePolicy::FailOpen.name(), "fail-open");
    assert_eq!(UnavailablePolicy::default(), UnavailablePolicy::FailClosed);
}

#[test]
fn the_state_handle_carries_the_stated_policy() {
    let store: std::sync::Arc<dyn BudgetStore> = std::sync::Arc::new(InProcessBudgetStore::new(8));
    let handle = crate::state::HttpStateHandle::with_budget_store(
        std::sync::Arc::clone(&store),
        UnavailablePolicy::FailOpen,
    );
    assert_eq!(handle.budget_policy(), UnavailablePolicy::FailOpen);

    let local = crate::state::HttpStateHandle::new();
    assert_eq!(
        local.budget_policy(),
        UnavailablePolicy::FailClosed,
        "the standing local table is fail-closed by construction"
    );
}
