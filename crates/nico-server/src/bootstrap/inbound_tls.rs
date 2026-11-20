//! A `tls` block: the certificate and key one listener presents.
//!
//! The block describes an inbound identity, not the roots an upstream handshake trusts. The latter
//! is `proxy.tls`, which is the opposite direction and a different section. Two places carry this
//! block — an `http.listeners[]` entry and the optional `metrics` section — so it lives here rather
//! than under either of them: both callers have to reach the same conclusion, because a present
//! block without both files is a configuration error and never "TLS off". An operator who wrote the
//! block meant to terminate TLS, and silently serving plaintext would be the worst possible
//! fallback.
//!
//! Validation is deliberately done while the bootstrap file is read. Pingora's path-based TLS
//! settings load the files again during `build()` and panic on a bad or changed pair; the service
//! that consumes these settings re-checks the identity before building its listener, so a file that
//! changes in between is a clean startup failure, not an abort inside the framework.

use std::{fs, path::PathBuf};

use nico_http::{InboundTls, validate_server_identity};
use serde::{Deserialize, Deserializer};

use super::BootstrapError;

/// One `tls` block exactly as written.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawTlsIdentity {
    cert_file: Option<PathBuf>,
    key_file: Option<PathBuf>,
}

/// Reads a `tls` key when it is present, including when its body is empty.
///
/// An empty `tls:` deserializes as `None`, which would be indistinguishable from an omitted block —
/// and an omitted block means plaintext. Mapping it to an empty block lets [`RawTlsIdentity::resolve`]
/// report the missing file instead of serving plaintext where the operator asked for TLS.
///
/// # Errors
/// Returns the deserializer's error when the block is not a mapping, or has unknown fields.
pub(super) fn section<'de, D>(deserializer: D) -> Result<Option<RawTlsIdentity>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<RawTlsIdentity>::deserialize(deserializer)?.or(Some(RawTlsIdentity::default())))
}

impl RawTlsIdentity {
    /// Reads and validates the identity this block names.
    ///
    /// `field` is the block's own dotted path — `http.listeners[2].tls` or `metrics.tls` — because
    /// the section that owns the block is the only one that knows how it is named: a listener entry
    /// has an index, the self-reporting section does not, and an error has to point at the place the
    /// operator wrote rather than at the section as a whole.
    ///
    /// # Errors
    /// Returns [`BootstrapError::InvalidField`] naming `{field}.cert_file` or `{field}.key_file`
    /// when a file is missing, unreadable, or not a usable certificate/key pair. Neither the files'
    /// contents nor the underlying decoder's payload become part of the message.
    pub(super) fn resolve(&self, field: &str) -> Result<InboundTls, BootstrapError> {
        let cert_file = required(field, &self.cert_file, "cert_file")?;
        let key_file = required(field, &self.key_file, "key_file")?;

        let certificate = fs::read(&cert_file).map_err(|source| {
            invalid(field, "cert_file", format!("could not be read: {source}"))
        })?;
        let key = fs::read(&key_file)
            .map_err(|source| invalid(field, "key_file", format!("could not be read: {source}")))?;

        validate_server_identity(&certificate, &key).map_err(|error| {
            invalid(
                field,
                error.field(),
                format!("is not a usable TLS identity: {error}"),
            )
        })?;
        Ok(InboundTls::from_bootstrap(cert_file, key_file))
    }
}

/// Returns one required path, or the field error naming it.
fn required(
    field: &str,
    path: &Option<PathBuf>,
    name: &'static str,
) -> Result<PathBuf, BootstrapError> {
    match path.as_ref().filter(|path| !path.as_os_str().is_empty()) {
        Some(path) => Ok(path.clone()),
        None => Err(invalid(
            field,
            name,
            "is required when the `tls` block is present",
        )),
    }
}

/// Builds the rejection for a field of one `tls` block.
fn invalid(
    field: &str,
    name: &'static str,
    reason: impl Into<std::borrow::Cow<'static, str>>,
) -> BootstrapError {
    BootstrapError::InvalidField {
        field: format!("{field}.{name}").into(),
        reason: reason.into(),
    }
}
