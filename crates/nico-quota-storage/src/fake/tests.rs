//! The fake's contract: it behaves as a `BudgetStore`, including failure injection.

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

fn key(seed: u8) -> BudgetKey {
    let mut k = [0u8; 32];
    k[0] = seed;
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

#[tokio::test]
async fn the_fake_opens_accumulates_and_refuses_like_a_store() {
    let backend = FakeBackend::new();
    let now = SystemTime::UNIX_EPOCH;
    let policy = policy(1000, 60);

    BudgetStore::charge(&backend, &key(1), &policy, &charge("c1", 600, now))
        .await
        .expect("the charge lands");
    BudgetStore::charge(&backend, &key(1), &policy, &charge("c2", 399, now))
        .await
        .expect("the charge lands");

    assert!(matches!(
        BudgetStore::check(&backend, &key(1), &policy, now).await,
        Ok(CheckResult::Allowed)
    ));
    BudgetStore::charge(&backend, &key(1), &policy, &charge("c3", 1, now))
        .await
        .expect("the crossing charge lands");
    assert!(matches!(
        BudgetStore::check(&backend, &key(1), &policy, now).await,
        Ok(CheckResult::Exceeded(_))
    ));
}

#[tokio::test]
async fn an_injected_failure_folds_into_the_domain_vocabulary() {
    let backend = FakeBackend::new();
    let now = SystemTime::UNIX_EPOCH;
    backend.fail_with(BackendError::Unavailable("the wire broke".into()));

    let error = BudgetStore::check(&backend, &key(1), &policy(1000, 60), now)
        .await
        .expect_err("the injected failure is returned");

    assert!(
        matches!(error, BudgetStoreError::Unavailable { .. }),
        "the fake's failure crosses as the domain's unavailable variant, never as Exceeded"
    );
}

#[tokio::test]
async fn clearing_the_failure_restores_the_backend() {
    let backend = FakeBackend::new();
    let now = SystemTime::UNIX_EPOCH;
    backend.fail_with(BackendError::Unavailable("down".into()));
    assert!(
        BudgetStore::check(&backend, &key(1), &policy(10, 60), now)
            .await
            .is_err()
    );

    backend.clear_failure();
    assert!(matches!(
        BudgetStore::check(&backend, &key(1), &policy(10, 60), now).await,
        Ok(CheckResult::Allowed)
    ));
}

#[tokio::test]
async fn a_charge_recorded_before_a_failure_survives_the_outage() {
    let backend = FakeBackend::new();
    let now = SystemTime::UNIX_EPOCH;
    let policy = policy(1000, 60);

    BudgetStore::charge(&backend, &key(1), &policy, &charge("c1", 500, now))
        .await
        .expect("the charge lands");
    backend.fail_with(BackendError::Unavailable("down".into()));
    assert!(
        BudgetStore::check(&backend, &key(1), &policy, now)
            .await
            .is_err(),
        "the outage answers the check"
    );
    backend.clear_failure();

    assert!(
        matches!(
            BudgetStore::check(&backend, &key(1), &policy, now).await,
            Ok(CheckResult::Allowed)
        ),
        "the recorded spend survives the outage: 500 of 1000"
    );
}

#[tokio::test]
async fn an_expired_window_resets_across_the_fake_boundary() {
    let backend = FakeBackend::new();
    let now = SystemTime::UNIX_EPOCH;
    let policy = policy(100, 60);

    BudgetStore::charge(&backend, &key(1), &policy, &charge("c1", 100, now))
        .await
        .expect("the charge lands");
    assert!(matches!(
        BudgetStore::check(&backend, &key(1), &policy, now).await,
        Ok(CheckResult::Exceeded(_))
    ));

    let later = now + Duration::from_secs(61);
    assert!(matches!(
        BudgetStore::check(&backend, &key(1), &policy, later).await,
        Ok(CheckResult::Allowed)
    ));
}
