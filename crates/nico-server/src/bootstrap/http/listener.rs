//! One `http.listeners[]` entry's `tls` block: the certificate and key that listener presents.
//!
//! The block describes the listener's identity, not the roots an upstream handshake trusts. The
//! latter is `proxy.tls`, which is a different direction and a different section. A present `tls`
//! block without both files is a configuration error rather than "TLS disabled": an operator who
//! wrote the block meant to terminate TLS on that listener, and silently serving plaintext would be
//! the worst possible fallback.
//!
//! Validation is deliberately done while the bootstrap file is read. Pingora's path-based TLS
//! settings load the files again during `build()` and panic on a bad or changed pair; the data plane
//! re-checks the identity before building the listener, so a file that changes in between is a clean
//! startup failure, not an abort inside the framework.

use std::{fs, path::PathBuf};

use nico_http::{InboundTls, validate_server_identity};
use serde::{Deserialize, Deserializer};

use super::super::BootstrapError;

/// One listener's `tls` block exactly as written.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawTlsIdentity {
    cert_file: Option<PathBuf>,
    key_file: Option<PathBuf>,
}

/// Reads a listener's `tls` key when it is present, including when its body is empty.
///
/// An empty `tls:` deserializes as `None`, which would be indistinguishable from an omitted block —
/// and an omitted block is a **plaintext** listener. Mapping it to an empty block lets
/// [`RawTlsIdentity::resolve`] report the missing file instead of serving plaintext where the
/// operator asked for TLS.
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
    /// Reads and validates the identity the listener at `index` presents.
    ///
    /// `index` is the entry's position in `http.listeners`, because that list is the only name a
    /// listener has before it is resolved: the error has to point at the entry the operator wrote,
    /// not at the section as a whole.
    ///
    /// # Errors
    /// Returns [`BootstrapError::InvalidField`] naming `http.listeners[index].tls.cert_file` or
    /// `http.listeners[index].tls.key_file` when a file is missing, unreadable, or not a usable
    /// certificate/key pair. Neither the files' contents nor the underlying decoder's payload become
    /// part of the message.
    pub(super) fn resolve(&self, index: usize) -> Result<InboundTls, BootstrapError> {
        let cert_file = required(index, &self.cert_file, "cert_file")?;
        let key_file = required(index, &self.key_file, "key_file")?;

        let certificate = fs::read(&cert_file).map_err(|source| {
            invalid(index, "cert_file", format!("could not be read: {source}"))
        })?;
        let key = fs::read(&key_file)
            .map_err(|source| invalid(index, "key_file", format!("could not be read: {source}")))?;

        validate_server_identity(&certificate, &key).map_err(|error| {
            invalid(
                index,
                error.field(),
                format!("is not a usable TLS identity: {error}"),
            )
        })?;
        Ok(InboundTls::from_bootstrap(cert_file, key_file))
    }
}

/// Returns one required path, or the field error naming it.
fn required(
    index: usize,
    path: &Option<PathBuf>,
    field: &'static str,
) -> Result<PathBuf, BootstrapError> {
    match path.as_ref().filter(|path| !path.as_os_str().is_empty()) {
        Some(path) => Ok(path.clone()),
        None => Err(invalid(
            index,
            field,
            "is required when a listener's `tls` block is present",
        )),
    }
}

/// Builds the rejection for a field of one listener's `tls` block.
fn invalid(
    index: usize,
    field: &'static str,
    reason: impl Into<std::borrow::Cow<'static, str>>,
) -> BootstrapError {
    BootstrapError::InvalidField {
        field: format!("http.listeners[{index}].tls.{field}").into(),
        reason: reason.into(),
    }
}
