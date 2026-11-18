//! The required `http` section of the bootstrap file: the listeners this process opens.
//!
//! A listener is a process fact, not configuration: its address, its transport (the presence of a
//! `tls` block) and the section-level `h2c` flag describe *this* process, so a control plane must
//! not be able to change them by pushing a document. They are read once, before any configuration
//! source exists.
//!
//! The section itself is **required**: an absent `http` block is a serde missing-field error, not a
//! default listener. A gateway that binds nothing serves nothing, so "which socket?" is a question
//! the operator answers rather than one the build answers for them. Inside the section every field
//! is validated as written; the accepted shapes belong to the data plane, which is why this module
//! only translates the written YAML into [`RawHttp`] and delegates the rules to
//! [`HttpSettings::resolve`].

use nico_http::{HttpSettings, HttpSettingsError, Listener, TlsPolicy, validate_listen};
use serde::Deserialize;

use super::BootstrapError;

mod listener;

/// The `http` section exactly as written, before validation.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawHttp {
    h2c: Option<bool>,
    // A list cannot express "at least one entry", so an empty list is a resolve-time error rather
    // than a parse error — which is also where the entry addresses are checked. `default` is what
    // makes an empty section report that field instead of serde's missing-field message.
    #[serde(default)]
    listeners: Vec<RawListener>,
}

/// One `http.listeners[]` entry exactly as written.
///
/// A single struct with an optional `tls` block rather than an untagged enum: `tls` being present
/// *is* the transport decision, and a missing one produces one readable error instead of the
/// untagged-enum "did not match any variant" message.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawListener {
    listen: String,
    // Present-but-empty is an error: an operator who wrote `tls:` meant TLS, not plaintext.
    #[serde(default, deserialize_with = "listener::section")]
    tls: Option<listener::RawTlsIdentity>,
}

impl RawHttp {
    /// Resolves the section into the settings the data plane runs with.
    ///
    /// `trust` is the outbound trust policy the bootstrap's own `proxy` section produced. It is not
    /// a field of this section (a bundle belongs to the process, not to a listener), but it is a
    /// field of the settings this section resolves, so it is handed over here.
    ///
    /// Each entry is checked here before it becomes a [`Listener`], so an unusable address is
    /// reported against the entry that carries it (`http.listeners[N].listen`) and a broken
    /// certificate against its block (`http.listeners[N].tls.*`). The data plane repeats both
    /// checks: it is the crate that decides what a listener may be, and a settings value built
    /// anywhere else must not be able to skip them.
    ///
    /// # Errors
    /// Returns [`BootstrapError::InvalidField`] under the `http.` prefix, naming the entry and the
    /// field the data plane rejected, or the rule it applied to the section as a whole.
    pub(super) fn resolve(&self, trust: TlsPolicy) -> Result<HttpSettings, BootstrapError> {
        if self.listeners.is_empty() {
            return Err(BootstrapError::InvalidField {
                field: "http.listeners".into(),
                reason: "must name at least one listener".into(),
            });
        }

        let mut listeners = Vec::with_capacity(self.listeners.len());
        for (index, raw) in self.listeners.iter().enumerate() {
            validate_listen(&raw.listen).map_err(|error| BootstrapError::InvalidField {
                field: format!("http.listeners[{index}].listen").into(),
                reason: error.reason().to_owned().into(),
            })?;
            listeners.push(match &raw.tls {
                Some(identity) => Listener::Tls {
                    listen: raw.listen.clone(),
                    tls: identity.resolve(index)?,
                },
                None => Listener::Plain {
                    listen: raw.listen.clone(),
                },
            });
        }

        HttpSettings::resolve(listeners, self.h2c, trust).map_err(|error: HttpSettingsError| {
            BootstrapError::InvalidField {
                field: format!("http.{}", error.field()).into(),
                reason: error.reason().to_owned().into(),
            }
        })
    }
}
