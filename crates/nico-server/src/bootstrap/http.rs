//! The optional `http` section of the bootstrap file.
//!
//! These are process knobs, not configuration: a listening address, plaintext HTTP/2, the two drain
//! windows and a worker-thread count describe *this* process, so a control plane must not be able to
//! change them by pushing a document. They are read once, before any configuration source exists.
//!
//! Every field is optional and the section itself may be omitted. A field that is **written** is
//! validated as written, even when it happens to equal its default, so a typo fails at startup
//! instead of being absorbed by the fallback; only an omitted field takes its default. The defaults
//! and the accepted ranges belong to the data plane, which is why this module only translates the
//! written YAML into [`RawHttp`] and delegates the rules to [`HttpSettings::resolve`].

use nico_http::{HttpSettings, HttpSettingsError, TlsPolicy};
use serde::Deserialize;

use super::BootstrapError;

mod tls;

/// The `http` section exactly as written, before defaults and validation.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawHttp {
    listen: Option<String>,
    h2c: Option<bool>,
    grace_period_ms: Option<u64>,
    graceful_shutdown_ms: Option<u64>,
    threads: Option<usize>,
    // Present-but-empty is an error: an operator who wrote `http.tls:` meant TLS, not plaintext.
    #[serde(default, deserialize_with = "tls::section")]
    tls: Option<tls::RawHttpTls>,
}

impl RawHttp {
    /// Resolves the section into the settings the data plane runs with.
    ///
    /// `tls` is the trust policy the bootstrap's own `tls` section produced. It is not a field of
    /// this section (a bundle belongs to the process, not to the listener), but it is a field of the
    /// settings this section resolves, so it is handed over here.
    ///
    /// The optional `http.tls` section is the listener's identity. It is read and validated here,
    /// once, before the listener is built; the data plane repeats the check when it assembles the
    /// service so a file that changes in between is refused rather than sent to Pingora's panicking
    /// loader.
    ///
    /// # Errors
    /// Returns [`BootstrapError::InvalidField`] under the `http.` prefix, naming the field the data
    /// plane rejected and the rule it applied.
    pub(super) fn resolve(&self, trust: TlsPolicy) -> Result<HttpSettings, BootstrapError> {
        let inbound = match &self.tls {
            Some(raw) => Some(raw.resolve()?),
            None => None,
        };
        HttpSettings::resolve_with_inbound_tls(
            self.listen.as_deref(),
            self.h2c,
            self.grace_period_ms,
            self.graceful_shutdown_ms,
            self.threads,
            trust,
            inbound,
        )
        .map_err(|error: HttpSettingsError| BootstrapError::InvalidField {
            field: format!("http.{}", error.field()).into(),
            reason: error.reason().to_owned().into(),
        })
    }
}
