//! A document entry compiled into the parameters a plugin runs with.
//!
//! Everything expensive or fallible happens here, once per publication: configuration is re-checked
//! into types the request path can use without checking, and the refusal the plugin presents is
//! compiled. A request then only reads these values, so no request can be the first to discover that
//! a document was unusable.
//!
//! Two invariants are carried in the types rather than in comments:
//!
//! - **No raw configuration values reach the request path.** A bucket size is a [`NonZeroU32`], a
//!   leeway is a [`Duration`], an identity source is this crate's closed [`KeyBy`]: a zero or an
//!   unknown spelling cannot become a division by zero or a wildcard arm later.
//! - **A credential is still a reference.** [`JwtKeyRef::secret_ref`] is what the document wrote;
//!   the materialized value is handed to the filter separately, so nothing in the compiled plan keeps
//!   a secret in a place a log or a debug print would reach by accident.

use std::{num::NonZeroU32, sync::Arc, time::Duration};

use nico_core::PluginSpec;

use crate::{
    PluginError,
    api_key::ApiKeyConfig,
    jwt::JwtConfig,
    rate_limit::{IdentitySource, LimitParameters, RateLimitConfig},
    rejection::{self, AuthRejections},
};

/// Largest accepted credential value, in bytes.
///
/// A credential larger than this is not a credential this build will read: the bound keeps a
/// mis-pointed `file:` reference from loading a whole file into a process that then holds it for the
/// lifetime of the generation.
pub(crate) const MAX_SECRET_BYTES: usize = 4 * 1024;

/// Largest accepted bearer token, in bytes, before anything is decoded.
///
/// Every later stage is bounded by this one: a token that is not even a plausible JWT is refused
/// before it is base64-decoded, split or parsed.
pub(crate) const MAX_TOKEN_BYTES: usize = 8 * 1024;

/// Largest accepted `sub` claim, in bytes.
///
/// The value becomes a log field and part of a rate-limit key, so it is bounded where it enters
/// rather than where it is used.
pub(crate) const MAX_SUBJECT_BYTES: usize = 256;

/// An HS256 key set plus the claims to enforce.
pub(crate) struct JwtSpec {
    /// The plugin instance name: it becomes the identity's `source` and the `authn` log field.
    pub(crate) instance: Arc<str>,
    /// The accepted keys, in document order.
    pub(crate) keys: Vec<JwtKeyRef>,
    /// The required `iss` claim, when the entry pins one.
    pub(crate) issuer: Option<Arc<str>>,
    /// The required `aud` claim, when the entry pins one.
    pub(crate) audience: Option<Arc<str>>,
    /// Accepted clock skew for `exp` and `nbf`.
    pub(crate) leeway: Duration,
    /// Both refusals this plugin presents.
    pub(crate) rejections: AuthRejections,
}

/// One accepted key: the `kid` a token may name and the reference to the shared secret.
pub(crate) struct JwtKeyRef {
    /// The `kid` this key answers to, when the entry pins one.
    pub(crate) kid: Option<Arc<str>>,
    /// The reference the secret was read from, kept for error messages only.
    pub(crate) secret_ref: Arc<str>,
}

impl JwtSpec {
    /// Compiles one `jwt` entry.
    ///
    /// The key set is non-empty by construction: [`JwtConfig::from_spec`] refuses an entry without
    /// keys before this function is reachable.
    ///
    /// # Errors
    /// - [`PluginError::InvalidParameter`] when a multi-key set has a key without a `kid`: a token
    ///   selects its key by `kid`, so such a key could never be selected and the credential an
    ///   operator wrote would silently never verify anything.
    /// - Every failure the entry's refusal presentation can have: an unknown template variable, an
    ///   unclosed placeholder, an unusable status, or a non-JSON content type without a body.
    pub(crate) fn compile(plugin: &PluginSpec, config: &JwtConfig) -> Result<Self, PluginError> {
        if config.keys.len() > 1
            && let Some(keyless) = config.keys.iter().find(|key| key.kid.is_none())
        {
            return Err(PluginError::InvalidParameter {
                plugin: plugin.name().to_owned(),
                field: "config.keys[].kid",
                reason: format!(
                    "must be set on every key of a multi-key set; the key reading `{}` could never be selected",
                    keyless.secret_ref
                )
                .into(),
            });
        }
        let instance = Arc::from(plugin.name());
        Ok(Self {
            instance,
            keys: config
                .keys
                .iter()
                .map(|key| JwtKeyRef {
                    kid: key.kid.as_deref().map(Arc::from),
                    secret_ref: Arc::from(key.secret_ref.as_str()),
                })
                .collect(),
            issuer: config.issuer.as_deref().map(Arc::from),
            audience: config.audience.as_deref().map(Arc::from),
            leeway: Duration::from_secs(u64::from(config.leeway_seconds())),
            rejections: rejection::auth(plugin.name(), plugin.error_response())?,
        })
    }

    /// Returns the credential references to materialize, in key order.
    ///
    /// Reading a credential is I/O, so it happens where the document is compiled: the materializer
    /// reads one value per reference and hands the list to [`JwtPlugin::new`] in this order. Keeping
    /// the references here means the kind's factory is the only thing that has to walk them.
    ///
    /// [`JwtPlugin::new`]: crate::jwt::JwtPlugin::new
    pub(crate) fn credentials(&self) -> impl Iterator<Item = &str> {
        self.keys.iter().map(|key| key.secret_ref.as_ref())
    }
}

/// A static key set plus the header the key is read from.
pub(crate) struct ApiKeySpec {
    /// The plugin instance name: it becomes the identity's `source` and the `authn` log field.
    pub(crate) instance: Arc<str>,
    /// The header the key arrives in, when the entry overrides `authorization`.
    ///
    /// `None` means `authorization` with a `Bearer` prefix; a configured header carries the key
    /// verbatim.
    pub(crate) header: Option<Arc<str>>,
    /// The accepted keys, in document order.
    pub(crate) entries: Vec<ApiKeyRef>,
    /// Both refusals this plugin presents.
    pub(crate) rejections: AuthRejections,
}

/// One accepted key: the name it authenticates as and the reference to its value.
pub(crate) struct ApiKeyRef {
    /// The subject reported when this key is presented.
    pub(crate) name: Arc<str>,
    /// The reference the key was read from, kept for error messages only.
    pub(crate) secret_ref: Arc<str>,
}

impl ApiKeySpec {
    /// Compiles one `api_key` entry.
    ///
    /// The key set is non-empty by construction: [`ApiKeyConfig::from_spec`] refuses an entry without
    /// keys before this function is reachable.
    ///
    /// # Errors
    /// Returns every failure the entry's refusal presentation can have: an unknown template
    /// variable, an unclosed placeholder, an unusable status, or a non-JSON content type without a
    /// body.
    pub(crate) fn compile(plugin: &PluginSpec, config: &ApiKeyConfig) -> Result<Self, PluginError> {
        let instance = Arc::from(plugin.name());
        Ok(Self {
            instance,
            header: config.header.as_deref().map(Arc::from),
            entries: config
                .keys
                .iter()
                .map(|key| ApiKeyRef {
                    name: Arc::from(key.name.as_str()),
                    secret_ref: Arc::from(key.secret_ref.as_str()),
                })
                .collect(),
            rejections: rejection::auth(plugin.name(), plugin.error_response())?,
        })
    }

    /// Returns the credential references to materialize, in key order.
    ///
    /// One value per reference is handed to [`ApiKeyPlugin::new`], and the entry's `name` becomes the
    /// subject the key authenticates as.
    ///
    /// [`ApiKeyPlugin::new`]: crate::api_key::ApiKeyPlugin::new
    pub(crate) fn credentials(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|entry| entry.secret_ref.as_ref())
    }
}

/// A token bucket per caller, with the key it is charged to and the refusal it answers with.
pub(crate) struct RateLimitSpec {
    /// The plugin instance name, for logs.
    pub(crate) instance: Arc<str>,
    /// The bucket's capacity, refill rate and table bound.
    ///
    /// The same value types the limiter holds, so the compiler can compare a published limiter with a
    /// newly compiled one and decide whether it may be reused across a reload.
    pub(crate) parameters: LimitParameters,
    /// Which identity the bucket key is derived from.
    pub(crate) key_by: KeyBy,
    /// The refusal this plugin presents when a bucket is empty.
    pub(crate) rejection: Arc<nico_http::Rejection>,
}

impl RateLimitSpec {
    /// Compiles one `rate_limit` entry.
    ///
    /// # Errors
    /// - [`PluginError::InvalidParameter`] for a zero `burst`, `per_second` or `max_keys`. The
    ///   configuration type refuses them too; this layer converts the numbers into the non-zero types
    ///   the limiter holds, and a value that cannot be converted must not become a panic later.
    /// - Every failure the entry's refusal presentation can have, as [`JwtSpec::compile`] lists them.
    pub(crate) fn compile(
        plugin: &PluginSpec,
        config: &RateLimitConfig,
    ) -> Result<Self, PluginError> {
        Ok(Self {
            instance: Arc::from(plugin.name()),
            parameters: LimitParameters::new(
                plugin.name(),
                config.burst,
                config.per_second,
                config.max_keys(),
            )?,
            key_by: KeyBy::compile(config.identity()),
            rejection: rejection::rate_limit(plugin.name(), plugin.error_response())?,
        })
    }

    /// Returns the bucket parameters this entry asks for.
    ///
    /// A compiler compares them with the published limiter's own: equal parameters mean the same
    /// buckets may keep serving, and a change means the quota changed and the buckets must start over.
    pub(crate) const fn parameters(&self) -> LimitParameters {
        self.parameters
    }
}

/// Which identity a rate-limit bucket is keyed by.
///
/// This crate's own closed enum, not [`IdentitySource`]: the document's spelling is parsed once, into
/// the configuration type, so the request path matches two variants and needs no wildcard arm. A
/// wildcard there would silently charge a future keying strategy to whatever the arm happened to
/// say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyBy {
    /// The identity an authentication plugin established, falling back to the client address.
    Subject,
    /// The client address, always.
    Ip,
}

impl KeyBy {
    /// Compiles the document's spelling.
    ///
    /// Infallible on purpose: an unknown spelling is refused while the entry's `config` is read, so
    /// every remaining value has a meaning here.
    const fn compile(source: IdentitySource) -> Self {
        match source {
            IdentitySource::Subject => Self::Subject,
            IdentitySource::Ip => Self::Ip,
        }
    }
}

/// Converts one non-zero configuration value, naming the plugin and field when it is zero.
///
/// # Errors
/// Returns [`PluginError::InvalidParameter`] when `value` is `0`.
pub(crate) fn nonzero(
    plugin: &str,
    field: &'static str,
    value: u32,
) -> Result<NonZeroU32, PluginError> {
    NonZeroU32::new(value).ok_or_else(|| PluginError::InvalidParameter {
        plugin: plugin.to_owned(),
        field,
        reason: "must be greater than zero".into(),
    })
}

#[cfg(test)]
mod tests;
