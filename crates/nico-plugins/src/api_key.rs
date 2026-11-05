//! Static API-key authentication.
//!
//! A key arrives either as `Authorization: Bearer <key>` (the default) or in the header the entry
//! names. It is compared against every configured key in constant time and the matching key's `name`
//! becomes the request's subject, so a rotation can keep two keys alive without changing what the
//! downstream is told about the caller.
//!
//! Two properties are deliberate:
//!
//! - **Every key is compared, always.** The loop has no early exit and no length shortcut: a refusal
//!   takes the same path whether the presented key is short, long, or one byte off the last key in
//!   the set. The only bound is the presented value's own length, which the caller already knows.
//! - **The key is never echoed.** A refusal carries a fixed sentence; the presented value appears in
//!   no error, no log field and no body.

mod config;
mod factory;

use std::sync::Arc;

use async_trait::async_trait;
// The kind's own configuration, reachable from the compiler because the document model hands it over
// as an opaque value: one path to it is easier to follow than two.
pub(crate) use config::ApiKeyConfig;
pub(crate) use factory::ApiKeyFactory;
use nico_http::{Outcome, Plugin, PluginContext, PluginError, PluginInfo, PluginResult, Rejection};
use subtle::{Choice, ConstantTimeEq};

use crate::{
    rejection::AuthRejections,
    spec::{ApiKeySpec, MAX_SECRET_BYTES},
};

/// The header a key is read from when the entry configures none.
const DEFAULT_HEADER: &str = "authorization";

/// A minimal configuration this kind accepts, as JSON text.
///
/// A key set is required — a plugin that accepts nobody is a misconfiguration, not a policy — so this
/// is a complete one-key example. [`INFO`] and the factory report the same text.
pub(crate) const DEFAULT_CONFIG: &str =
    r#"{"keys":[{"name":"client","secret_ref":"env:API_KEY"}]}"#;

/// What an operator can learn about this kind before enabling it.
pub(crate) const INFO: PluginInfo = PluginInfo::new(
    env!("CARGO_PKG_VERSION"),
    "static API key authentication",
    DEFAULT_CONFIG,
);

/// Why a request carried no credential.
const NO_CREDENTIAL: &str = "the request carries no api key";
/// Why a credential could not be used.
const NOT_ACCEPTED: &str = "the api key is not accepted";

/// One accepted key: the name it authenticates as and its value.
struct Entry {
    name: Arc<str>,
    secret: Arc<[u8]>,
}

/// One `api_key` policy instance, shared by every request of every route that references it.
pub(crate) struct ApiKeyPlugin {
    /// The plugin instance name: it becomes the identity's `source` and the `authn` log field.
    instance: Arc<str>,
    /// The header the key arrives in, when the entry overrides `authorization`.
    header: Option<Arc<str>>,
    keys: Vec<Entry>,
    rejections: AuthRejections,
}

impl ApiKeyPlugin {
    /// Builds one filter from its compiled entry and the materialized keys, in entry order.
    ///
    /// # Errors
    /// Returns [`PluginError::CredentialCountMismatch`] when `secrets` does not hold one value per
    /// declared key.
    pub(crate) fn new(
        spec: &ApiKeySpec,
        secrets: Vec<Arc<[u8]>>,
    ) -> Result<Self, crate::PluginError> {
        if secrets.len() != spec.entries.len() {
            return Err(crate::PluginError::CredentialCountMismatch {
                plugin: spec.instance.to_string(),
                declared: spec.entries.len(),
                materialized: secrets.len(),
            });
        }
        Ok(Self {
            instance: Arc::clone(&spec.instance),
            header: spec.header.as_ref().map(Arc::clone),
            keys: spec
                .entries
                .iter()
                .zip(secrets)
                .map(|(entry, secret)| Entry {
                    name: Arc::clone(&entry.name),
                    secret,
                })
                .collect(),
            rejections: spec.rejections.clone(),
        })
    }

    /// Returns the key the request presented, when it presented one.
    ///
    /// A configured header carries the key verbatim: an operator who chose `x-api-key` did not ask for
    /// a scheme prefix, and accepting one would make the documented shape a lie. The default header is
    /// `authorization`, where the key is the credential of a bearer scheme.
    fn presented(&self, ctx: &dyn PluginContext) -> Result<Option<String>, PluginError> {
        let header = self.header.as_deref().unwrap_or(DEFAULT_HEADER);
        let value = ctx.request_header(header)?;
        match self.header {
            Some(_) => Ok(value),
            None => Ok(value.as_deref().and_then(bearer).map(str::to_owned)),
        }
    }

    /// Returns the entry `presented` matches, comparing every configured key.
    fn find(&self, presented: &[u8]) -> Option<&Entry> {
        if presented.len() > MAX_SECRET_BYTES {
            // No configured key can be this long (each was materialized under the same bound), so the
            // answer is known without comparing: it also bounds the work one request can ask for.
            return None;
        }
        let mut hit: Option<&Entry> = None;
        for entry in &self.keys {
            let matched = bool::from(constant_time_eq(&entry.secret, presented));
            // The assignment, not a `break`, records the match: every key is compared even after one
            // has matched, so the time a refusal takes says nothing about the set's contents.
            hit = if matched { hit.or(Some(entry)) } else { hit };
        }
        hit
    }

    /// Builds the refusal `detail` describes.
    fn refuse(&self, detail: &'static str, rejection: Arc<Rejection>) -> PluginError {
        tracing::debug!(
            plugin = %self.instance,
            reason = detail,
            "the request was not authenticated"
        );
        PluginError::Refused {
            rejection,
            detail,
            retry_after: None,
        }
    }
}

#[async_trait]
impl Plugin for ApiKeyPlugin {
    fn name(&self) -> &str {
        &self.instance
    }

    fn info(&self) -> PluginInfo {
        INFO
    }

    async fn on_request(&self, ctx: &mut dyn PluginContext) -> PluginResult {
        let Some(presented) = self.presented(ctx)? else {
            return Err(self.refuse(NO_CREDENTIAL, self.rejections.missing()));
        };
        let Some(entry) = self.find(presented.as_bytes()) else {
            return Err(self.refuse(NOT_ACCEPTED, self.rejections.invalid()));
        };
        // The key's own name is the subject: it is what an operator configured and what a downstream
        // can be told, while the key itself never leaves this comparison.
        ctx.set_identity(&entry.name)?;
        Ok(Outcome::Continue)
    }
}

/// Compares a configured key with a presented one in constant time, length included.
///
/// `subtle`'s slice comparison short-circuits on a different length, so the length decision is taken
/// here as a `Choice` of its own and folded in; the content is compared over the shorter of the two,
/// which cannot read past either slice. A truncated presentation therefore reaches the same code as a
/// complete one that is wrong in a byte.
fn constant_time_eq(expected: &[u8], presented: &[u8]) -> Choice {
    let expected_len = u64::try_from(expected.len()).unwrap_or(u64::MAX);
    let presented_len = u64::try_from(presented.len()).unwrap_or(u64::MAX);
    let length = expected_len.ct_eq(&presented_len);
    let shorter = expected.len().min(presented.len());
    let content = expected[..shorter].ct_eq(&presented[..shorter]);
    length & content
}

/// Returns the `Authorization` header's bearer key, when it presents one.
///
/// The scheme is case-insensitive (RFC 6750 §2.1); anything else means no key was presented, which is
/// a different answer from a key that is not accepted.
fn bearer(value: &str) -> Option<&str> {
    let (scheme, key) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let key = key.trim();
    if key.is_empty() || key.contains(' ') {
        return None;
    }
    Some(key)
}

// Compiled-in proof that a filter and the refusal it shares can cross `.await` and threads.
const _: fn() = || {
    const fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<ApiKeyPlugin>();
};

#[cfg(test)]
mod tests;
