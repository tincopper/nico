//! The optional `http.tls` section: the certificate and key this listener presents.
//!
//! The section describes the listener's identity, not the roots an upstream handshake trusts. The
//! latter is the top-level `tls` section, which is unchanged. A present `http.tls` without both files
//! is a configuration error rather than "TLS disabled": an operator who wrote the section meant to
//! terminate TLS, and silently serving plaintext would be the worst possible fallback.
//!
//! Validation is deliberately done while the bootstrap file is read. Pingora's path-based TLS
//! settings load the files again during `build()` and panic on a bad or changed pair; the data plane
//! re-checks the identity before building the listener, so a file that changes in between is a clean
//! startup failure, not an abort inside the framework.

use std::{fs, path::PathBuf};

use nico_http::{InboundTls, validate_server_identity};
use serde::{Deserialize, Deserializer};

use super::BootstrapError;

/// The `http.tls` section exactly as written.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawHttpTls {
    cert_file: Option<PathBuf>,
    key_file: Option<PathBuf>,
}

/// Reads the `tls` key when it is present, including when its body is empty.
///
/// An empty `http.tls:` deserializes as `None`, which would be indistinguishable from an omitted
/// section; mapping it to an empty section lets [`RawHttpTls::resolve`] report the missing file.
///
/// # Errors
/// Returns the deserializer's error when the section is not a mapping, or has unknown fields.
pub(super) fn section<'de, D>(deserializer: D) -> Result<Option<RawHttpTls>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<RawHttpTls>::deserialize(deserializer)?.or(Some(RawHttpTls::default())))
}

impl RawHttpTls {
    /// Reads and validates the identity the listener presents.
    ///
    /// # Errors
    /// Returns [`BootstrapError::InvalidField`] naming `http.tls.cert_file` or
    /// `http.tls.key_file` when a file is missing, unreadable, or not a usable certificate/key pair.
    /// Neither the files' contents nor the underlying decoder's payload become part of the message.
    pub(super) fn resolve(&self) -> Result<InboundTls, BootstrapError> {
        let cert_file = required(&self.cert_file, "cert_file")?;
        let key_file = required(&self.key_file, "key_file")?;

        let certificate = fs::read(&cert_file)
            .map_err(|source| invalid("cert_file", format!("could not be read: {source}")))?;
        let key = fs::read(&key_file)
            .map_err(|source| invalid("key_file", format!("could not be read: {source}")))?;

        validate_server_identity(&certificate, &key).map_err(|error| {
            invalid(
                error.field(),
                format!("is not a usable TLS identity: {error}"),
            )
        })?;
        Ok(InboundTls::from_bootstrap(cert_file, key_file))
    }
}

/// Returns one required path, or the field error naming it.
fn required(path: &Option<PathBuf>, field: &'static str) -> Result<PathBuf, BootstrapError> {
    match path.as_ref().filter(|path| !path.as_os_str().is_empty()) {
        Some(path) => Ok(path.clone()),
        None => Err(invalid(field, "is required when `http.tls` is present")),
    }
}

/// Builds the rejection for a field under the `http.tls.` prefix.
fn invalid(
    field: &'static str,
    reason: impl Into<std::borrow::Cow<'static, str>>,
) -> BootstrapError {
    BootstrapError::InvalidField {
        field: format!("http.tls.{field}").into(),
        reason: reason.into(),
    }
}
