//! The multi-replica contract: two independent clients sharing one window through the fake.
//!
//! These are the proofs the deployment promise rests on: two `HttpStateHandle`-equivalent
//! holders of the same backend see one window, an exhausted window refuses the next replica's
//! admission, and a charge the first replica could not confirm during an outage is retried
//! exactly once by its id.

use std::time::{Duration, SystemTime};

use nico_http::{
    BudgetKey, BudgetPolicy, BudgetStore, BudgetStoreError, Charge, ChargeId, CheckResult,
};

use crate::{fake::FakeBackend, seam::BackendError};

fn policy(limit: u64, window: u32) -> BudgetPolicy {
    BudgetPolicy {
        limit_tokens: limit,
        window_seconds: window,
    }
}

fn key(route: &str, identity: &str, tenant: bool) -> BudgetKey {
    // The same hash the request path derives; here, the values' distinctness is the fact.
    let mut k = [0u8; 32];
    let seed = format!("{route}|{identity}|{tenant}");
    for (index, byte) in seed.bytes().enumerate().take(32) {
        k[index] = byte;
    }
    k
}

fn charge(id: &str, tokens: u64, at: SystemTime) -> Charge {
    Charge {
        id: ChargeId(id.into()),
        input: tokens,
        output: 0,
        at,
    }
}

async fn check(
    store: &dyn BudgetStore,
    key: &BudgetKey,
    policy: &BudgetPolicy,
    now: SystemTime,
) -> Result<CheckResult, BudgetStoreError> {
    BudgetStore::check(store, key, policy, now).await
}

async fn record(
    store: &dyn BudgetStore,
    key: &BudgetKey,
    policy: &BudgetPolicy,
    charge: &Charge,
) -> Result<(), BudgetStoreError> {
    BudgetStore::charge(store, key, policy, charge).await
}

#[tokio::test]
async fn two_replicas_share_one_tenant_window() {
    let backend = FakeBackend::new();
    let now = SystemTime::UNIX_EPOCH;
    let policy = policy(500, 60);
    let tenant_key = key("llm-chat", "tenant-acme", true);

    // Replica A serves a request that spends the window.
    record(&backend, &tenant_key, &policy, &charge("r1-c1", 500, now))
        .await
        .expect("replica A's charge lands");

    // Replica B's next admission sees replica A's spend.
    assert!(
        matches!(
            check(&backend, &tenant_key, &policy, now).await,
            Ok(CheckResult::Exceeded(_))
        ),
        "replica B refuses: the shared window is spent"
    );
}

#[tokio::test]
async fn two_replicas_accumulate_one_subject_window_across_concurrent_charges() {
    let backend = FakeBackend::new();
    let now = SystemTime::UNIX_EPOCH;
    let policy = policy(1_000, 60);
    let subject_key = key("llm-chat", "user-1", false);

    // Two replicas charge concurrently; neither increment may be lost.
    let first_charge = charge("r1", 400, now);
    let second_charge = charge("r2", 400, now);
    let (first, second) = tokio::join!(
        record(&backend, &subject_key, &policy, &first_charge),
        record(&backend, &subject_key, &policy, &second_charge),
    );
    first.expect("replica A's charge lands");
    second.expect("replica B's charge lands");

    assert!(
        matches!(
            check(&backend, &subject_key, &policy, now).await,
            Ok(CheckResult::Allowed)
        ),
        "800 of 1000: both increments were recorded"
    );
    record(&backend, &subject_key, &policy, &charge("r3", 200, now))
        .await
        .expect("the crossing charge lands");
    assert!(
        matches!(
            check(&backend, &subject_key, &policy, now).await,
            Ok(CheckResult::Exceeded(_))
        ),
        "1000 spent: the shared window is spent"
    );
}

#[tokio::test]
async fn different_tenants_and_routes_stay_isolated_across_replicas() {
    let backend = FakeBackend::new();
    let now = SystemTime::UNIX_EPOCH;
    let policy = policy(500, 60);

    let tenant_a = key("llm-chat", "tenant-a", true);
    let tenant_b = key("llm-chat", "tenant-b", true);
    let other_route = key("llm-embed", "tenant-a", true);
    record(&backend, &tenant_a, &policy, &charge("c1", 500, now))
        .await
        .expect("the charge lands");

    assert!(
        matches!(
            check(&backend, &tenant_b, &policy, now).await,
            Ok(CheckResult::Allowed)
        ),
        "another tenant's window is its own"
    );
    assert!(
        matches!(
            check(&backend, &other_route, &policy, now).await,
            Ok(CheckResult::Allowed)
        ),
        "another route's window is its own"
    );
}

#[tokio::test]
async fn a_charge_failed_during_an_outage_is_retried_under_the_same_id() {
    let backend = FakeBackend::new();
    let now = SystemTime::UNIX_EPOCH;
    let policy = policy(1_000, 60);
    let subject_key = key("llm-chat", "user-1", false);

    // Replica A charges while the backend is up, then the backend goes down before the
    // confirmation reaches it.
    record(
        &backend,
        &subject_key,
        &policy,
        &charge("shared-charge", 600, now),
    )
    .await
    .expect("replica A's charge lands");
    backend.fail_with(BackendError::Unavailable("the wire broke".into()));
    assert!(
        check(&backend, &subject_key, &policy, now).await.is_err(),
        "the outage answers replica B's admission check"
    );
    backend.clear_failure();

    // Replica A retries the unconfirmed charge under the same id: it must bill once.
    record(
        &backend,
        &subject_key,
        &policy,
        &charge("shared-charge", 600, now),
    )
    .await
    .expect("the retried charge is accepted");
    assert!(
        matches!(
            check(&backend, &subject_key, &policy, now).await,
            Ok(CheckResult::Allowed)
        ),
        "600 recorded once, not twice: the retry was a duplicate id"
    );
}

#[tokio::test]
async fn a_window_that_expires_across_replicas_resets_for_both() {
    let backend = FakeBackend::new();
    let now = SystemTime::UNIX_EPOCH;
    let policy = policy(500, 60);
    let tenant_key = key("llm-chat", "tenant-acme", true);

    record(&backend, &tenant_key, &policy, &charge("r1", 500, now))
        .await
        .expect("the charge lands");
    assert!(matches!(
        check(&backend, &tenant_key, &policy, now).await,
        Ok(CheckResult::Exceeded(_))
    ));

    // The next window: whichever replica's request arrives first opens the fresh window,
    // and both replicas see it from then on.
    let later = now + Duration::from_secs(61);
    assert!(matches!(
        check(&backend, &tenant_key, &policy, later).await,
        Ok(CheckResult::Allowed)
    ));
    assert!(matches!(
        check(&backend, &tenant_key, &policy, later).await,
        Ok(CheckResult::Allowed)
    ));
}
