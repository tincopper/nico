//! The `api_key` kind's own configuration, read from the entry's `config` value.
//!
//! The document model hands this crate the value the entry wrote under `config` and knows nothing
//! about the fields below; they are checked here, while the document is compiled, with
//! `deny_unknown_fields` so a typo is refused rather than ignored.
//!
//! Two of the rules are about what the values become rather than about their shape: a key is a
//! *reference* (`env:`/`file:`), never a value, and an entry's `name` becomes the request identity
//! and a log field, so it is restricted to short printable ASCII — a newline there would let one
//! document forge a second log record.

use nico_core::{PluginSpec, is_secret_ref};
use serde::Deserialize;

use crate::{PluginError, invalid_parameter};

/// Largest accepted API key name, in bytes.
const MAX_LABEL_BYTES: usize = 63;
/// Largest accepted custom header name, in bytes.
const MAX_HEADER_BYTES: usize = 64;

/// One `api_key` entry's configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ApiKeyConfig {
    /// The request header the key arrives in, when the entry overrides `authorization`.
    pub(crate) header: Option<String>,
    /// The accepted keys, in document order.
    ///
    /// Defaulted so an entry that writes no key set reaches [`ApiKeyConfig::validate`] and is refused
    /// there: "wrote nothing" and "wrote an empty set" are the same mistake and deserve one message.
    #[serde(default)]
    pub(crate) keys: Vec<ApiKeyEntryConfig>,
}

/// One accepted key: the name it authenticates as, optional tenant quota grouping, and reference to its value.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ApiKeyEntryConfig {
    /// The subject reported when this key is presented.
    pub(crate) name: String,
    /// The tenant whose route-scoped token budget this key shares, when configured.
    #[serde(default)]
    pub(crate) tenant: Option<String>,
    /// The reference the key is read from, for example `env:PARTNER_KEY`.
    pub(crate) secret_ref: String,
}

impl ApiKeyConfig {
    /// Reads one entry's `config` value and checks every rule of this kind.
    ///
    /// An entry that writes no `config` is read as one that writes no fields: the empty key set that
    /// follows is refused, which is the point — a plugin that accepts nobody is a misconfiguration,
    /// not a policy.
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

    /// Checks the header name, the key set and every key name.
    ///
    /// # Errors
    /// - [`PluginError::InvalidParameter`] for an empty key set, a header name that is not a
    ///   lowercase token, or a key name that is empty, too long, not printable ASCII, or repeated.
    pub(crate) fn validate(&self, plugin: &str) -> Result<(), PluginError> {
        if self.keys.is_empty() {
            return Err(invalid_parameter(
                plugin,
                "config.keys",
                "must not be empty",
            ));
        }

        if let Some(header) = self.header.as_deref() {
            validate_header_name(plugin, header)?;
        }

        let mut names: Vec<&str> = Vec::with_capacity(self.keys.len());
        for key in &self.keys {
            validate_label(plugin, "config.keys[].name", &key.name)?;
            if let Some(tenant) = key.tenant.as_deref() {
                validate_label(plugin, "config.keys[].tenant", tenant)?;
            }
            if names.contains(&key.name.as_str()) {
                return Err(invalid_parameter(
                    plugin,
                    "config.keys[].name",
                    format!("`{}` names more than one key", key.name),
                ));
            }
            names.push(&key.name);
            if !is_secret_ref(&key.secret_ref) {
                return Err(invalid_parameter(
                    plugin,
                    "config.keys[].secret_ref",
                    "must be an `env:` or `file:` reference",
                ));
            }
        }
        Ok(())
    }
}

/// Checks a custom request header name: lowercase, because that is how headers are matched.
///
/// # Errors
/// Returns [`PluginError::InvalidParameter`] when `value` is empty, longer than
/// [`MAX_HEADER_BYTES`], or not made of `a-z`, `0-9` and `-`.
fn validate_header_name(plugin: &str, value: &str) -> Result<(), PluginError> {
    const FIELD: &str = "config.header";
    if value.is_empty() {
        return Err(invalid_parameter(plugin, FIELD, "must not be empty"));
    }
    if value.len() > MAX_HEADER_BYTES {
        return Err(invalid_parameter(
            plugin,
            FIELD,
            format!("must be at most {MAX_HEADER_BYTES} bytes"),
        ));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(invalid_parameter(
            plugin,
            FIELD,
            "must be a lowercase header name (`a-z`, `0-9` and `-`)",
        ));
    }
    Ok(())
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
