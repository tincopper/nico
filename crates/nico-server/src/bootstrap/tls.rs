//! The optional `tls` section of the bootstrap file: the roots an upstream handshake trusts.
//!
//! Trust roots describe *this process*, not a route: they are read once at startup and Pingora
//! bakes them into its client connector, so a reload of the document cannot change them. That is
//! why they live in the bootstrap file, next to the listener and the drain windows, and never in a
//! configuration source.
//!
//! An omitted section means the platform certificate store. A **present** section is a complete
//! replacement for it — rustls uses a configured bundle *instead of* the native roots — so the
//! operator has to concatenate what they want to trust into one file. The section is documented as
//! replace-not-append for the same reason: an operator who assumed "add" would silently lose the
//! public roots.
//!
//! Validation happens here, at startup, and it is strict on purpose: Pingora's connector
//! construction `unwrap()`s its own parse of `ca_file`, so a bad file that reached it would abort
//! the process inside `HttpProxy::new` with no field name and no clean exit.
//!
//! What this module deliberately does **not** check is the platform store. Parsing a document must
//! not depend on the host it is parsed on, or every unit test of this module would be hostage to the
//! machine's trust store. "Can this environment serve with the roots it has?" — including the
//! platform store, and a bundle that changed between these two reads — is the assembly's question,
//! answered by `nico_http::ensure_usable` in `serve_with`.

use std::{borrow::Cow, fs, path::PathBuf};

use nico_http::{TlsPolicy, validate_bundle};
use serde::{Deserialize, Deserializer};

use super::BootstrapError;

/// The `tls` section exactly as written, before validation.
///
/// Every field is optional so that `ca_file`'s absence is reported as a missing field rather than
/// as a missing section; the resolver refuses it either way.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawTls {
    ca_file: Option<PathBuf>,
}

/// Reads the `tls` key when it is present, including when its body is empty.
///
/// A section with no body (`tls:` and nothing under it) deserializes as `None`, which would be
/// indistinguishable from an omitted section — and an operator who wrote the section and forgot the
/// path would believe a bundle was configured while the platform roots stayed in force. Mapping the
/// empty body to an empty section makes the resolver report it as `tls.ca_file`.
///
/// # Errors
/// Returns the deserializer's error when the section is not a mapping, or has unknown fields.
pub(super) fn section<'de, D>(deserializer: D) -> Result<Option<RawTls>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<RawTls>::deserialize(deserializer)?.or(Some(RawTls::default())))
}

impl RawTls {
    /// Resolves the section into the policy the data plane is built with.
    ///
    /// The file is read once, here: the policy stores its path, and Pingora opens that path again
    /// while building the connector. A file whose content changes in between is therefore a startup
    /// failure, not a surprise on the first request.
    ///
    /// # Errors
    /// Returns [`BootstrapError::InvalidField`] under `tls.ca_file` when the section names no file,
    /// when the file cannot be read, or when its bytes are not a usable set of trust roots.
    pub(super) fn resolve(&self) -> Result<TlsPolicy, BootstrapError> {
        let Some(path) = self
            .ca_file
            .as_deref()
            .filter(|path| !path.as_os_str().is_empty())
        else {
            return Err(invalid(
                "the `tls` section is present, so it must name a `ca_file`",
            ));
        };
        let bytes =
            fs::read(path).map_err(|source| invalid(format!("could not be read: {source}")))?;
        // The refusals are the data plane's own wording: it is the crate that knows which parse the
        // client performs, and its message names what to fix (an empty file, a key, a bad section).
        validate_bundle(&bytes).map_err(|error| invalid(error.to_string()))?;
        Ok(TlsPolicy::from_bootstrap(Some(path.to_owned())))
    }
}

/// Builds the rejection for a bundle this process cannot use.
fn invalid(reason: impl Into<Cow<'static, str>>) -> BootstrapError {
    BootstrapError::InvalidField {
        field: "tls.ca_file".into(),
        reason: reason.into(),
    }
}
