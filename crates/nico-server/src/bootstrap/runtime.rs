//! The optional `runtime` section of the bootstrap file: the knobs the process runs with.
//!
//! A worker-thread count and the two shutdown windows describe *this* process, not the protocol it
//! serves and not a route it serves it for, so a control plane must not be able to change them by
//! pushing a document. They are read once, before any configuration source exists — the same reason
//! the listeners live here.
//!
//! Every field is optional and the section itself may be omitted. A field that is **written** is
//! validated as written, even when it happens to equal its default, so a typo fails at startup
//! instead of being absorbed by the fallback; only an omitted field takes its default. The defaults
//! and the accepted ranges belong to the data plane, which is why this module only translates the
//! written YAML into [`RawRuntime`] and delegates the rules to [`ServerSettings::resolve`].

use nico_http::{ServerSettings, ServerSettingsError};
use serde::Deserialize;

use super::BootstrapError;

/// The `runtime` section exactly as written, before defaults and validation.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawRuntime {
    threads: Option<usize>,
    grace_period_ms: Option<u64>,
    graceful_shutdown_ms: Option<u64>,
}

impl RawRuntime {
    /// Resolves the section into the settings the process runs with.
    ///
    /// # Errors
    /// Returns [`BootstrapError::InvalidField`] under the `runtime.` prefix, naming the field the
    /// data plane rejected and the rule it applied. The reason quotes the bound the check used, so
    /// the message cannot drift from the constant.
    pub(super) fn resolve(&self) -> Result<ServerSettings, BootstrapError> {
        ServerSettings::resolve(
            self.threads,
            self.grace_period_ms,
            self.graceful_shutdown_ms,
        )
        .map_err(|error: ServerSettingsError| BootstrapError::InvalidField {
            field: format!("runtime.{}", error.field()).into(),
            reason: error.reason().to_owned().into(),
        })
    }
}
