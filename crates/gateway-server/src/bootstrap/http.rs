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

use gateway_http::{HttpSettings, HttpSettingsError};
use serde::Deserialize;

use super::BootstrapError;

/// The `http` section exactly as written, before defaults and validation.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawHttp {
    listen: Option<String>,
    h2c: Option<bool>,
    grace_period_ms: Option<u64>,
    graceful_shutdown_ms: Option<u64>,
    threads: Option<usize>,
}

impl RawHttp {
    /// Resolves the section into the settings the data plane runs with.
    ///
    /// # Errors
    /// Returns [`BootstrapError::InvalidField`] under the `http.` prefix, naming the field the data
    /// plane rejected and the rule it applied.
    pub(super) fn resolve(&self) -> Result<HttpSettings, BootstrapError> {
        HttpSettings::resolve(
            self.listen.as_deref(),
            self.h2c,
            self.grace_period_ms,
            self.graceful_shutdown_ms,
            self.threads,
        )
        .map_err(|error: HttpSettingsError| BootstrapError::InvalidField {
            field: format!("http.{}", error.field()).into(),
            reason: error.reason().to_owned().into(),
        })
    }
}
