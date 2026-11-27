//! The etcd adapter: one transaction per admission or charge.
//!
//! A window is one etcd key under the configured prefix, holding one JSON
//! [`WindowRecord`]. Every check and every charge is a compare-and-put transaction:
//! compare the stored value against the bytes this client just read, put the updated
//! record when the compare holds, and retry from a fresh read when another replica won
//! the race. etcd serializes transactions on its revision, so two replicas cannot
//! interleave half of a decision — the property the seam's contract requires and the
//! reason the read-modify-write lives inside the server, not in this client.
//!
//! Expiry is a record fact, not a lease. A window's `started` timestamp travels in the
//! value, so an expired window is simply replaced on the next operation; that keeps one
//! window to one key with no lease keep-alive task, and a replica whose clock is slightly
//! ahead cannot expire another's live window (the seam's skew tolerance).
//!
//! The wire client is `etcd_client::Client` behind a tokio mutex, the same shape the
//! config source's real client uses: operations are driven from the caller's runtime.

use std::{sync::Arc, time::SystemTime};

use async_trait::async_trait;
use etcd_client::{Client, Compare, CompareOp, GetOptions, Txn, TxnOp};
use nico_http::{
    BudgetKey, BudgetPolicy, BudgetStore, BudgetStoreError, Charge, ChargeId, CheckResult,
};
use tokio::sync::Mutex;

use crate::seam::{BackendError, CheckOutcome, WindowRecord, apply_charge, apply_check};

/// How many compare-and-put rounds one operation may spend before reporting contention.
const MAX_TXN_ATTEMPTS: u32 = 8;

/// The etcd-backed budget store.
pub struct EtcdBudgetStore {
    /// The prefix every budget key is written under, as the bootstrap stated it.
    prefix: Arc<str>,
    /// The multiplexed connection. `etcd-client`'s operations need `&mut`, so the client
    /// sits behind a mutex like the config source's real client does.
    client: Mutex<Client>,
}

impl std::fmt::Debug for EtcdBudgetStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EtcdBudgetStore")
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

impl EtcdBudgetStore {
    /// Connects the store to one etcd cluster with the wire options the assembly built
    /// (TLS material and credentials are the caller's: this adapter is transport-agnostic).
    ///
    /// # Errors
    /// Returns the wire error when the endpoints cannot be reached or the credentials are
    /// refused.
    pub async fn connect(
        endpoints: &[String],
        key_prefix: &str,
        wire_options: Option<etcd_client::ConnectOptions>,
    ) -> Result<Self, BudgetStoreError> {
        let client = Client::connect(endpoints, wire_options)
            .await
            .map_err(|error| BudgetStoreError::Unavailable {
                source: Box::new(error),
            })?;
        Ok(Self {
            prefix: Arc::from(key_prefix),
            client: Mutex::new(client),
        })
    }

    /// The etcd key one budget window lives at: the prefix plus the opaque key's hex.
    fn storage_key(&self, key: &BudgetKey) -> Vec<u8> {
        let mut storage = Vec::with_capacity(self.prefix.len() + key.len() * 2);
        storage.extend_from_slice(self.prefix.as_bytes());
        for byte in key {
            storage.extend_from_slice(format!("{byte:02x}").as_bytes());
        }
        storage
    }

    /// Reads and decodes one stored record, treating an absent key as no window.
    async fn read_record(
        &self,
        client: &mut Client,
        storage_key: &[u8],
    ) -> Result<Option<(Vec<u8>, WindowRecord)>, BudgetStoreError> {
        let response = client
            .get(storage_key, Some(GetOptions::new()))
            .await
            .map_err(wire_unavailable)?;
        match response.kvs().first() {
            None => Ok(None),
            Some(kv) => {
                let bytes = kv.value().to_vec();
                let record = decode(&bytes)
                    .map_err(|corrupt| BackendError::Corrupt(corrupt).into_domain())?;
                Ok(Some((bytes, record)))
            }
        }
    }

    /// Runs one read–apply–compare-and-put round, retrying from a fresh read when another
    /// replica's write won the compare.
    async fn txn_apply<T>(
        &self,
        key: &BudgetKey,
        apply: impl Fn(Option<WindowRecord>) -> Result<(T, Option<WindowRecord>), BudgetStoreError>,
    ) -> Result<T, BudgetStoreError> {
        let storage_key = self.storage_key(key);
        let mut client = self.client.lock().await;
        for _attempt in 0..MAX_TXN_ATTEMPTS {
            let Some((previous, stored)) = self.read_record(&mut client, &storage_key).await?
            else {
                let (verdict, record) = apply(None)?;
                let Some(record) = record else {
                    return Ok(verdict);
                };
                let bytes = encode(&record)?;
                // Create only if the key is still absent (version == 0): a record that
                // appeared since the read is a newer window this round must not clobber.
                let txn = Txn::new()
                    .when(vec![Compare::version(
                        storage_key.clone(),
                        CompareOp::Equal,
                        0,
                    )])
                    .and_then(vec![TxnOp::put(storage_key.clone(), bytes, None)]);
                if self.run_txn(&mut client, txn).await? {
                    return Ok(verdict);
                }
                continue;
            };

            let (verdict, record) = apply(Some(stored))?;
            let Some(record) = record else {
                return Ok(verdict);
            };
            let bytes = encode(&record)?;
            let txn = Txn::new()
                .when(vec![Compare::value(
                    storage_key.clone(),
                    CompareOp::Equal,
                    previous,
                )])
                .and_then(vec![TxnOp::put(storage_key.clone(), bytes, None)]);
            if self.run_txn(&mut client, txn).await? {
                return Ok(verdict);
            }
            // Another replica wrote between the read and the compare; re-read and re-apply.
        }
        Err(BudgetStoreError::Conflict {
            source: "the budget window stayed contended longer than the adapter allows".into(),
        })
    }

    /// Runs one transaction, folding wire failures into the store's vocabulary.
    async fn run_txn(&self, client: &mut Client, txn: Txn) -> Result<bool, BudgetStoreError> {
        let response = client.txn(txn).await.map_err(wire_unavailable)?;
        Ok(response.succeeded())
    }
}

/// Folds a wire failure into the domain's unavailable variant.
fn wire_unavailable(error: etcd_client::Error) -> BudgetStoreError {
    BudgetStoreError::Unavailable {
        source: Box::new(error),
    }
}

/// The wire shape of one stored window: what etcd's value holds.
///
/// A DTO rather than a derive on the seam's types: `ChargeId` is a domain vocabulary that
/// stays free of any storage encoding, and the wire can evolve (a versioned namespace)
/// without touching the domain contract.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StoredWindow {
    used: u64,
    started: SystemTime,
    charges: Vec<String>,
}

impl StoredWindow {
    fn from_record(record: &WindowRecord) -> Self {
        Self {
            used: record.used,
            started: record.started,
            charges: record
                .charges
                .iter()
                .map(|id| id.0.as_ref().to_owned())
                .collect(),
        }
    }

    fn into_record(self) -> WindowRecord {
        WindowRecord {
            used: self.used,
            started: self.started,
            charges: self
                .charges
                .into_iter()
                .map(|id| ChargeId(id.into()))
                .collect(),
        }
    }
}

/// Encodes one record as JSON.
fn encode(record: &WindowRecord) -> Result<Vec<u8>, BudgetStoreError> {
    serde_json::to_vec(&StoredWindow::from_record(record)).map_err(|error| {
        BudgetStoreError::Invalid {
            source: Box::new(error),
        }
    })
}

/// Decodes one stored record.
fn decode(bytes: &[u8]) -> Result<WindowRecord, crate::seam::CorruptRecord> {
    serde_json::from_slice::<StoredWindow>(bytes)
        .map(StoredWindow::into_record)
        .map_err(|_| crate::seam::CorruptRecord {
            reason: "the stored bytes are not a window record this build reads",
        })
}

#[async_trait]
impl BudgetStore for EtcdBudgetStore {
    async fn check(
        &self,
        key: &BudgetKey,
        policy: &BudgetPolicy,
        now: SystemTime,
    ) -> Result<CheckResult, BudgetStoreError> {
        self.txn_apply(key, |stored| {
            let applied = apply_check(stored, policy, now)
                .map_err(|corrupt| BackendError::Corrupt(corrupt).into_domain())?;
            Ok((
                match applied.outcome {
                    CheckOutcome::Allowed => CheckResult::Allowed,
                    CheckOutcome::Spent => CheckResult::Exceeded(nico_http::Exceeded {
                        limit: policy.limit_tokens,
                        window_seconds: policy.window_seconds,
                    }),
                },
                applied.stored,
            ))
        })
        .await
    }

    async fn charge(
        &self,
        key: &BudgetKey,
        policy: &BudgetPolicy,
        charge: &Charge,
    ) -> Result<(), BudgetStoreError> {
        self.txn_apply(key, |stored| {
            let applied = apply_charge(stored, policy, &charge.id, charge.tokens(), charge.at)
                .map_err(|corrupt| BackendError::Corrupt(corrupt).into_domain())?;
            Ok(((), Some(applied.stored)))
        })
        .await
    }
}
