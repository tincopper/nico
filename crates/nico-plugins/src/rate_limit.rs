//! Rate limiting: one token bucket per caller, kept in a bounded table.
//!
//! The bucket is charged to whatever the entry asks for: an identity an authentication plugin
//! established, or a hash of the client's address. The identity is read from the request's typed slot —
//! never from a header — so a client cannot pick the bucket it is charged to by writing one.
//!
//! Three facts make this a limit rather than a suggestion:
//!
//! - **Order is the compiler's job.** The plugin only spends a token when it runs, and its factory
//!   declares the `Limit` class, so the compiler places every authentication plugin before it: a
//!   document that writes the limit first would otherwise let an unauthenticated request consume a
//!   token of some other caller's quota.
//! - **The table is bounded.** A new key is one bucket, the table holds at most `max_keys` of them and
//!   evicts the least recently used — a caller that keeps changing its key cannot grow memory.
//! - **A refusal says when to come back.** A token bucket always knows: the wait is how long the
//!   bucket needs to hold one token again, and the writer rounds it up to the whole seconds
//!   `Retry-After` is written in.
//!
//! The table outlives a reload, so it belongs to the kind's factory ([`RateLimitFactory`]) rather than
//! to a publication: republishing a document must not hand every caller a fresh quota.

mod config;
mod factory;
mod limiter;

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
// Re-exported inside the crate because the compiler and the filter both need the two types, and one
// path to them is easier to follow than two.
pub(crate) use config::{IdentitySource, RateLimitConfig};
pub(crate) use factory::RateLimitFactory;
pub(crate) use limiter::{LimitParameters, Limiter};
use nico_http::{Outcome, Plugin, PluginContext, PluginError, PluginInfo, PluginResult, Rejection};
use sha2::{Digest, Sha256};
use tokio::time::Instant;

use crate::spec::{KeyBy, RateLimitSpec};

/// Why the request was refused, for the log line.
const DETAIL: &str = "the caller is over its rate limit";

/// A minimal configuration this kind accepts, as JSON text.
///
/// `burst` and `per_second` are required — a bucket cannot guess its own size — while `max_keys` and
/// `identity` fall back to the kind's defaults. [`INFO`] and the factory report the same text.
pub(crate) const DEFAULT_CONFIG: &str = r#"{"burst":100,"per_second":50}"#;

/// What an operator can learn about this kind before enabling it.
pub(crate) const INFO: PluginInfo = PluginInfo::new(
    env!("CARGO_PKG_VERSION"),
    "token-bucket rate limiting per caller",
    DEFAULT_CONFIG,
);

/// Tag that distinguishes the two key kinds inside one digest.
///
/// A subject and a client address could otherwise produce the same pre-image and share a bucket.
const KIND_SUBJECT: u8 = 1;
const KIND_PEER: u8 = 2;

/// Tag that distinguishes an address hash from "this session has no address at all".
const PEER_KNOWN: u8 = 1;
const PEER_UNKNOWN: u8 = 0;

/// One rate-limit policy instance, shared by every request of every route that references it.
pub(crate) struct RateLimitPlugin {
    /// The plugin instance name, for logs.
    instance: Arc<str>,
    /// The shared bucket table, which outlives a reload while the parameters do not change.
    limiter: Arc<Limiter>,
    /// Which identity the bucket key is derived from.
    key_by: KeyBy,
    /// The compiled refusal.
    rejection: Arc<Rejection>,
}

impl RateLimitPlugin {
    /// Builds one filter over a limiter the compiler owns.
    ///
    /// The limiter is passed in rather than created here because it is the state that survives a
    /// reload: the compiler keeps the `Arc<Limiter>` for a plugin whose parameters did not change and
    /// builds a new one when they did.
    #[must_use]
    pub(crate) fn new(spec: &RateLimitSpec, limiter: Arc<Limiter>) -> Self {
        Self {
            instance: Arc::clone(&spec.instance),
            limiter,
            key_by: spec.key_by,
            rejection: Arc::clone(&spec.rejection),
        }
    }

    /// Derives the bucket this request is charged to.
    ///
    /// The digest is the whole key: the table then holds one fixed-width entry per bucket, a request
    /// allocates nothing to look one up, and neither a subject nor a caller's address is stored in
    /// plaintext. Three facts always go in — the route, this instance's name and the caller — so two
    /// routes and two instances can never share a bucket even though each instance owns its own table.
    /// A subject is used only when the entry asks for it *and* an authentication plugin established
    /// one; anything else falls back to the address hash, and a session with no address shares one
    /// bucket per route — fail-closed, since it can only be served more slowly than configured.
    fn bucket_key(&self, ctx: &dyn PluginContext) -> [u8; 32] {
        let mut hasher = Sha256::new();
        feed(&mut hasher, ctx.route_id().unwrap_or_default());
        feed(&mut hasher, &self.instance);
        match (self.key_by, ctx.identity()) {
            (KeyBy::Subject, Some(subject)) => {
                hasher.update([KIND_SUBJECT]);
                feed(&mut hasher, subject);
            }
            _ => {
                hasher.update([KIND_PEER]);
                match ctx.client_ip_hash() {
                    // The context hands over a hash, not an address: this digest only has to make two
                    // callers look different, and the process never needs the address again.
                    Some(hash) => {
                        hasher.update([PEER_KNOWN]);
                        hasher.update(hash.to_be_bytes());
                    }
                    None => hasher.update([PEER_UNKNOWN]),
                }
            }
        }
        hasher.finalize().into()
    }

    /// Builds the refusal for a bucket that is empty for `wait` longer.
    fn reject(&self, wait: Duration) -> PluginError {
        PluginError::Refused {
            rejection: Arc::clone(&self.rejection),
            detail: DETAIL,
            retry_after: Some(wait),
        }
    }
}

#[async_trait]
impl Plugin for RateLimitPlugin {
    fn name(&self) -> &str {
        &self.instance
    }

    fn info(&self) -> PluginInfo {
        INFO
    }

    async fn on_request(&self, ctx: &mut dyn PluginContext) -> PluginResult {
        // The limiter's clock is Tokio's, so a paused test drives it and a request never sees a wall
        // clock step as a refill.
        match self.limiter.acquire(self.bucket_key(ctx), Instant::now()) {
            Ok(()) => Ok(Outcome::Continue),
            Err(wait) => {
                tracing::debug!(
                    plugin = %self.instance,
                    retry_after_ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX),
                    "the request is over its rate limit"
                );
                Err(self.reject(wait))
            }
        }
    }
}

/// Feeds one variable-length field into the key digest, length-prefixed.
///
/// Without the prefix, `("ab", "c")` and `("a", "bc")` would hash the same bytes and share a bucket.
fn feed(hasher: &mut Sha256, part: &str) {
    hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
    hasher.update(part.as_bytes());
}

// Compiled-in proof that the state a compiler keeps across publications is thread-safe.
const _: fn() = || {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Limiter>();
    assert_send_sync::<RateLimitPlugin>();
    assert_send_sync::<LimitParameters>();
};

#[cfg(test)]
mod tests;
