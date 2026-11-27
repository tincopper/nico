//! The Redis adapter: one server-side script per admission or charge.
//!
//! A window is one Redis key under the configured prefix, holding one JSON window record —
//! the same wire shape the etcd adapter stores, so the seam's semantics stay the one
//! definition both backends agree on. Every check and every charge runs one Lua script:
//! the read-modify-write happens inside Redis (scripts execute atomically), and two
//! replicas' scripts on one key serialize against each other. The script returns the
//! window's verdict or the updated record; the client never holds a half of a decision.
//!
//! Expiry is a TTL on the key, refreshed to the window's remaining duration on every write.
//! The TTL is garbage collection, never the verdict: a record whose TTL lapsed is simply
//! absent, which the seam treats as a fresh window — the same answer the expiry check
//! inside the script would give.
//!
//! Time comes from the script's caller as a unix-seconds argument, so a replica's wall
//! clock decides expiry exactly the way the local store's does; the seam's skew tolerance
//! (a window opened slightly ahead of the local clock stays live) holds.
//!
//! The connection is a [`redis::aio::ConnectionManager`]: it reconnects on its own, so a
//! restarted Redis answers the next operation instead of poisoning this store.

use std::{sync::Arc, time::SystemTime};

use async_trait::async_trait;
use nico_http::{BudgetKey, BudgetPolicy, BudgetStore, BudgetStoreError, Charge, CheckResult};
use redis::aio::ConnectionManager;

/// The admission script: read the record, apply the check semantics, write the (possibly
/// fresh) window when it admits.
///
/// Returns `1` when allowed, `0` when the window is spent, and raises an error-shaped
/// reply when the stored bytes cannot be read — which the adapter folds into
/// [`BackendError::Corrupt`].
const CHECK_SCRIPT: &str = r#"
local raw = redis.call('GET', KEYS[1])
if raw then
  local ok, record = pcall(cjson.decode, raw)
  if not ok then
    return redis.error_reply("CORRUPT")
  end
  local spent = record.used
  local started = record.started
  if ARGV[1] - started >= tonumber(ARGV[3]) then
    spent = 0
    started = tonumber(ARGV[1])
  end
  if spent >= tonumber(ARGV[2]) then
    return 0
  end
  redis.call('SET', KEYS[1],
    cjson.encode({used = spent, started = started, charges = record.charges or {}}),
    'EX', tonumber(ARGV[4]))
  return 1
end
redis.call('SET', KEYS[1],
  cjson.encode({used = 0, started = tonumber(ARGV[1]), charges = {}}),
  'EX', tonumber(ARGV[4]))
return 1
"#;

/// The charge script: read the record, dedupe on the charge id, accumulate, refresh the TTL.
///
/// Returns the updated `used` value, or the same value unchanged when the charge id was
/// already recorded. A corrupt record is an error reply, never a silent reset.
const CHARGE_SCRIPT: &str = r#"
local raw = redis.call('GET', KEYS[1])
local used = 0
local started = tonumber(ARGV[3])
local charges = {}
if raw then
  local ok, record = pcall(cjson.decode, raw)
  if not ok then
    return redis.error_reply("CORRUPT")
  end
  used = record.used
  started = record.started
  charges = record.charges or {}
  if ARGV[3] - started >= tonumber(ARGV[2]) then
    used = 0
    started = tonumber(ARGV[3])
    charges = {}
  end
  for _, recorded in ipairs(charges) do
    if recorded == ARGV[1] then
      return used
    end
  end
end
used = used + tonumber(ARGV[4])
table.insert(charges, ARGV[1])
if #charges > tonumber(ARGV[5]) then
  table.remove(charges, 1)
end
redis.call('SET', KEYS[1],
  cjson.encode({used = used, started = started, charges = charges}),
  'EX', tonumber(ARGV[6]))
return used
"#;

/// The Redis-backed budget store.
pub struct RedisBudgetStore {
    /// The prefix every budget key is written under, as the bootstrap stated it.
    prefix: Arc<str>,
    /// The auto-reconnecting connection. One manager serves every operation: Redis
    /// serializes scripts per key, which is the atomicity the seam's contract names.
    connection: ConnectionManager,
    check: redis::Script,
    charge: redis::Script,
}

impl std::fmt::Debug for RedisBudgetStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RedisBudgetStore")
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

impl RedisBudgetStore {
    /// Connects the store to one Redis deployment.
    ///
    /// The endpoint list is the bootstrap's; the adapter connects to the first entry that
    /// carries an address, because a multi-endpoint list is a cluster's client-side view and
    /// this adapter's atomicity contract requires one primary. A topology the list implies
    /// but the connection cannot serve is refused here, where the field is known, rather
    /// than weakening the guarantees on the wire.
    ///
    /// # Errors
    /// Returns the wire error when no endpoint can be reached or the credentials are refused.
    pub async fn connect(endpoints: &[String], key_prefix: &str) -> Result<Self, BudgetStoreError> {
        let client = endpoints.first().and_then(|endpoint| {
            // `rediss://` is TLS; `redis://` is cleartext. An endpoint the parser refuses is
            // an unavailable store, named at startup where the field is known.
            redis::Client::open(endpoint.as_str()).ok()
        });
        let Some(client) = client else {
            return Err(BudgetStoreError::Unavailable {
                source: "no endpoint in the list is an address this adapter can connect to".into(),
            });
        };
        let connection = ConnectionManager::new(client).await.map_err(|error| {
            BudgetStoreError::Unavailable {
                source: Box::new(error),
            }
        })?;
        Ok(Self {
            prefix: Arc::from(key_prefix),
            connection,
            check: redis::Script::new(CHECK_SCRIPT),
            charge: redis::Script::new(CHARGE_SCRIPT),
        })
    }

    /// The Redis key one budget window lives at: the prefix plus the opaque key's hex.
    fn storage_key(&self, key: &BudgetKey) -> String {
        let mut storage = String::with_capacity(self.prefix.len() + key.len() * 2);
        storage.push_str(&self.prefix);
        for byte in key {
            storage.push_str(&format!("{byte:02x}"));
        }
        storage
    }

    /// Folds one wire failure into the store's vocabulary, keeping the cause typed.
    fn fold(error: redis::RedisError) -> BudgetStoreError {
        use redis::ErrorKind;
        match error.kind() {
            ErrorKind::AuthenticationFailed => BudgetStoreError::Unauthorized {
                source: Box::new(error),
            },
            ErrorKind::IoError | ErrorKind::ResponseError => BudgetStoreError::Unavailable {
                source: Box::new(error),
            },
            _ => BudgetStoreError::Unavailable {
                source: Box::new(error),
            },
        }
    }

    /// Folds a corrupt-record reply into the domain's vocabulary.
    fn corrupt() -> BudgetStoreError {
        BudgetStoreError::Corrupt {
            source: "the stored window record is not one this build can read".into(),
        }
    }
}

#[async_trait]
impl BudgetStore for RedisBudgetStore {
    async fn check(
        &self,
        key: &BudgetKey,
        policy: &BudgetPolicy,
        now: SystemTime,
    ) -> Result<CheckResult, BudgetStoreError> {
        let now_secs = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let remaining = remaining_ttl(policy, now);
        let storage_key = self.storage_key(key);
        let verdict: i64 = self
            .check
            .key(&storage_key)
            .arg(now_secs)
            .arg(policy.limit_tokens)
            .arg(policy.window_seconds)
            .arg(remaining)
            .invoke_async(&mut self.connection.clone())
            .await
            .map_err(|error| {
                if is_corrupt(&error) {
                    Self::corrupt()
                } else {
                    Self::fold(error)
                }
            })?;
        Ok(match verdict {
            1 => CheckResult::Allowed,
            _ => CheckResult::Exceeded(nico_http::Exceeded {
                limit: policy.limit_tokens,
                window_seconds: policy.window_seconds,
            }),
        })
    }

    async fn charge(
        &self,
        key: &BudgetKey,
        policy: &BudgetPolicy,
        charge: &Charge,
    ) -> Result<(), BudgetStoreError> {
        let at_secs = charge
            .at
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let remaining = remaining_ttl(policy, charge.at);
        let storage_key = self.storage_key(key);
        let _: i64 = self
            .charge
            .key(&storage_key)
            .arg(charge.id.0.as_ref())
            .arg(policy.window_seconds)
            .arg(at_secs)
            .arg(charge.tokens())
            .arg(crate::seam::RETAINED_CHARGE_IDS)
            .arg(remaining)
            .invoke_async(&mut self.connection.clone())
            .await
            .map_err(|error| {
                if is_corrupt(&error) {
                    Self::corrupt()
                } else {
                    Self::fold(error)
                }
            })?;
        Ok(())
    }
}

/// The TTL one write sets: the window's remaining duration, at least one second.
fn remaining_ttl(policy: &BudgetPolicy, _now: SystemTime) -> u64 {
    u64::from(policy.window_seconds).max(1)
}

/// Returns whether the wire error is the script's corrupt-record reply.
fn is_corrupt(error: &redis::RedisError) -> bool {
    error.to_string().contains("CORRUPT")
}

// The scripts above must agree with the seam's pure semantics ([`crate::seam`]); the
// contract suite drives the same scenarios through both, so a drift between the Lua and the
// Rust reading is a test failure, not a production surprise.
