//! The `rate_limit` kind's own configuration, read from the entry's `config` value.
//!
//! A bucket size and a refill rate are this kind's numbers, not the document model's: the envelope
//! hands the value over untouched and the rules below decide what a usable quota is. They are checked
//! while the document is compiled, so a request never discovers a zero-rate bucket.
//!
//! The defaults live here too, next to the type that carries them: an entry that omits `max_keys`
//! still gets a bounded table, and an entry that omits the key source still charges the identity an
//! authentication plugin established.

use nico_core::PluginSpec;
use serde::Deserialize;

use crate::{PluginError, invalid_parameter};

/// Largest accepted `max_keys`; the table that honours it is bounded by this ceiling.
pub(crate) const MAX_KEYS_CEILING: u32 = 100_000;

/// Default for `max_keys` when the entry omits it.
pub(crate) const DEFAULT_MAX_KEYS: u32 = 10_000;

/// One `rate_limit` entry's configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RateLimitConfig {
    /// The bucket capacity: how many requests may arrive back to back.
    pub(crate) burst: u32,
    /// How many tokens one second restores.
    pub(crate) per_second: u32,
    /// The table bound, when the entry overrides [`DEFAULT_MAX_KEYS`].
    #[serde(default)]
    pub(crate) max_keys: Option<u32>,
    /// Which identity the bucket key is derived from, when the entry overrides [`IdentitySource::Subject`].
    #[serde(default)]
    pub(crate) identity: Option<IdentitySource>,
}

/// Which identity a rate-limit bucket is keyed by.
///
/// It lives with the kind that uses it rather than in the document model: a key source is a
/// rate-limit decision, not something every plugin shares. The set is closed, so an unknown spelling
/// is refused while the entry's `config` is read — earlier than any validation rule below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum IdentitySource {
    /// Key by the identity an authentication plugin established, when one did.
    Subject,
    /// Key by a hash of the client address.
    Ip,
}

impl RateLimitConfig {
    /// Reads one entry's `config` value and checks every rule of this kind.
    ///
    /// # Errors
    /// - [`PluginError::InvalidConfig`] when the value has a field this kind does not have, a field
    ///   of the wrong type, an unknown `identity` spelling, or a `config` that is not a mapping.
    /// - [`PluginError::InvalidParameter`] for a zero `burst` or `per_second`, or a `max_keys` outside
    ///   `1..=100000`; the message names the field below `config`.
    pub(crate) fn from_spec(spec: &PluginSpec) -> Result<Self, PluginError> {
        let config: Self = crate::config_value(spec)?;
        config.validate(spec.name())?;
        Ok(config)
    }

    /// Returns the maximum number of buckets the table may hold, applying the default when omitted.
    pub(crate) const fn max_keys(&self) -> u32 {
        match self.max_keys {
            Some(max_keys) => max_keys,
            None => DEFAULT_MAX_KEYS,
        }
    }

    /// Returns which identity the bucket key is derived from, applying the default when omitted.
    pub(crate) const fn identity(&self) -> IdentitySource {
        match self.identity {
            Some(source) => source,
            None => IdentitySource::Subject,
        }
    }

    /// Checks the numbers a bucket cannot work with.
    ///
    /// # Errors
    /// Returns [`PluginError::InvalidParameter`] naming the field when `burst` or `per_second` is
    /// `0`, or when `max_keys` is outside `1..=100000`.
    pub(crate) fn validate(&self, plugin: &str) -> Result<(), PluginError> {
        for (field, value) in [
            ("config.burst", self.burst),
            ("config.per_second", self.per_second),
        ] {
            if value == 0 {
                return Err(invalid_parameter(
                    plugin,
                    field,
                    "must be greater than zero",
                ));
            }
        }
        if !(1..=MAX_KEYS_CEILING).contains(&self.max_keys()) {
            return Err(invalid_parameter(
                plugin,
                "config.max_keys",
                format!("must be in 1..={MAX_KEYS_CEILING}"),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
