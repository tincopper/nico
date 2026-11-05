//! The `jwt` kind's own configuration, read from the entry's `config` value.
//!
//! This is the other half of the document model's split: `nico_core` owns the envelope (`name`,
//! `kind`, `enabled`, `buffers_body`, `error_response`) and hands the kind's fields over untouched,
//! so the field names, defaults and accepted ranges below are the `jwt` kind's alone. They are
//! checked while the document is compiled — the same publication step, the same last-known-good
//! behaviour — with `deny_unknown_fields`, so a typo under `config` is still refused rather than
//! ignored.
//!
//! Two rules are about what a key *is*, not about its shape: a key is a reference (`env:`/`file:`),
//! never a value, and its `kid` is written into a log record, so it is restricted to short printable
//! ASCII. A newline there would let one document forge a second log line.

use nico_core::{PluginSpec, is_secret_ref};
use serde::Deserialize;

use crate::{PluginError, invalid_parameter};

/// Accepted clock skew for `exp` and `nbf` when the entry does not configure one.
pub(crate) const DEFAULT_LEEWAY_SECONDS: u32 = 30;

/// Largest accepted `kid`, in bytes.
const MAX_LABEL_BYTES: usize = 63;

/// One `jwt` entry's configuration.
///
/// Fields are public inside the crate: this is the parsed form of a document value, and the compiler
/// reads it directly instead of going through an accessor per field.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JwtConfig {
    /// The accepted keys, in document order.
    ///
    /// Defaulted so an entry that writes no key set reaches [`JwtConfig::validate`] and is refused
    /// there: "wrote nothing" and "wrote an empty set" are the same mistake and deserve one message.
    #[serde(default)]
    pub(crate) keys: Vec<JwtKeyConfig>,
    /// The required `iss` claim, when the entry pins one.
    pub(crate) issuer: Option<String>,
    /// The required `aud` claim, when the entry pins one.
    pub(crate) audience: Option<String>,
    /// The accepted clock skew, when the entry overrides [`DEFAULT_LEEWAY_SECONDS`].
    #[serde(default)]
    pub(crate) leeway_seconds: Option<u32>,
}

/// One accepted key: the identifier a token may name and the reference to the shared secret.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JwtKeyConfig {
    /// The `kid` this key answers to, when the entry pins one.
    pub(crate) kid: Option<String>,
    /// The reference the secret is read from, for example `env:JWT_SECRET`.
    pub(crate) secret_ref: String,
}

impl JwtConfig {
    /// Reads one entry's `config` value and checks every rule of this kind.
    ///
    /// An entry that writes no `config` is read as one that writes no fields, so every `default`
    /// below applies and the validation still runs — "wrote nothing" must be refused exactly like
    /// "wrote an empty key set", otherwise an incomplete entry would compile into a plugin that
    /// accepts nobody.
    ///
    /// # Errors
    /// - [`PluginError::InvalidConfig`] when the value has a field this kind does not have, a field
    ///   of the wrong type, or a `config` that is not a mapping.
    /// - [`PluginError::InvalidParameter`] when a value is outside the range this kind accepts; the
    ///   message names the field below `config`.
    pub(crate) fn from_spec(spec: &PluginSpec) -> Result<Self, PluginError> {
        let config: Self = crate::config_value(spec)?;
        config.validate(spec.name())?;
        Ok(config)
    }

    /// Returns the accepted clock skew, applying the default when the entry omits one.
    pub(crate) const fn leeway_seconds(&self) -> u32 {
        match self.leeway_seconds {
            Some(seconds) => seconds,
            None => DEFAULT_LEEWAY_SECONDS,
        }
    }

    /// Checks the key set and every `kid`.
    ///
    /// # Errors
    /// - [`PluginError::InvalidParameter`] for an empty key set, a `secret_ref` that is not a
    ///   reference, or a `kid` that is empty, too long, not printable ASCII, or repeated.
    pub(crate) fn validate(&self, plugin: &str) -> Result<(), PluginError> {
        if self.keys.is_empty() {
            return Err(invalid_parameter(
                plugin,
                "config.keys",
                "must not be empty",
            ));
        }
        // One pass, so the first key that is wrong is the one reported. The `kid`s are collected in a
        // vector rather than a set: a key set is a handful of entries, and keeping document order makes
        // the "which one" in a duplicate message predictable.
        let mut kids: Vec<&str> = Vec::with_capacity(self.keys.len());
        for key in &self.keys {
            if !is_secret_ref(&key.secret_ref) {
                return Err(invalid_parameter(
                    plugin,
                    "config.keys[].secret_ref",
                    "must be an `env:` or `file:` reference",
                ));
            }
            let Some(kid) = key.kid.as_deref() else {
                continue;
            };
            validate_label(plugin, "config.keys[].kid", kid)?;
            if kids.contains(&kid) {
                return Err(invalid_parameter(
                    plugin,
                    "config.keys[].kid",
                    format!("`{kid}` names more than one key"),
                ));
            }
            kids.push(kid);
        }
        Ok(())
    }
}

/// Checks a short value that is written into a log record: non-empty printable ASCII.
///
/// # Errors
/// Returns [`PluginError::InvalidParameter`] naming `field` when `value` is empty, longer than
/// [`MAX_LABEL_BYTES`], or carries a character that could break a log record.
fn validate_label(plugin: &str, field: &'static str, value: &str) -> Result<(), PluginError> {
    if value.is_empty() {
        return Err(invalid_parameter(plugin, field, "must not be empty"));
    }
    if value.len() > MAX_LABEL_BYTES {
        return Err(invalid_parameter(
            plugin,
            field,
            format!("must be at most {MAX_LABEL_BYTES} bytes"),
        ));
    }
    if !value.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(invalid_parameter(
            plugin,
            field,
            "must contain printable ASCII without whitespace",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
