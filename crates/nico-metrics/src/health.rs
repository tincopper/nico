//! The readiness projection: what a probe is told about this process.
//!
//! Liveness and readiness are two different questions, and only one of them reads shared state.
//! [`Readiness::live`] answers "this process can answer" from the fact that it answered, while
//! [`Readiness::ready`] answers "this process can serve a request" from the caller's own verdict.
//! A configuration failure must never make a healthy process look dead, so the live endpoint
//! cannot read the configuration state.

use nico_core::{ConfigStatus, ErrorCategory};
use serde::Serialize;

use crate::response::Response;

/// The readiness facts one probe request is answered from.
///
/// `servable` is computed by the caller: this crate does not know what "can serve a request" means
/// in the data plane, so it cannot read the published state itself. The status is the
/// configuration-state snapshot the body reports.
pub struct Readiness<'a> {
    servable: bool,
    status: &'a ConfigStatus,
}

impl<'a> Readiness<'a> {
    /// Builds the facts for one request.
    #[must_use]
    pub fn new(servable: bool, status: &'a ConfigStatus) -> Self {
        Self { servable, status }
    }

    /// Returns the configuration status the readiness body is rendered from.
    pub(crate) fn status(&self) -> &'a ConfigStatus {
        self.status
    }

    /// Answers `GET /health/live`: the process is alive because it answered.
    ///
    /// No shared state is read, so a configuration failure cannot turn a healthy process into a
    /// restart loop.
    #[must_use]
    pub fn live(&self) -> Response {
        Response::text(200, "text/plain", "ok")
    }

    /// Answers `GET /health/ready`: `200` exactly when the caller said the process can serve.
    ///
    /// `degraded` and `stale` do not change the status: serving a last-known-good snapshot is a
    /// designed behaviour, and the fields report it. A deployment that wants "not fresh" to mean
    /// "do not send traffic" reads `config_degraded` instead, which is what the gauge is for.
    #[must_use]
    pub fn ready(&self) -> Response {
        let status = self.status;
        let body = ReadyBody {
            ready: self.servable,
            generation: status.generation,
            revision: status.revision,
            digest: status.digest.as_deref(),
            stale: status.stale,
            degraded: status.degraded,
            last_error_category: status.last_error_category,
            last_reload_unix_secs: status.last_reload_unix_secs,
        };
        // Every field is a plain value, so serialization cannot fail. The status line already
        // carries the verdict a probe reads; the body is the detail.
        let json = serde_json::to_string(&body)
            .unwrap_or_else(|_error| format!("{{\"ready\":{}}}", self.servable));
        let code = if self.servable { 200 } else { 503 };
        Response::text(code, "application/json", json)
    }
}

/// The `/health/ready` body, in the field order the contract asserts on.
#[derive(Debug, Serialize)]
struct ReadyBody<'a> {
    ready: bool,
    generation: u64,
    revision: Option<u64>,
    digest: Option<&'a str>,
    stale: bool,
    degraded: bool,
    last_error_category: Option<ErrorCategory>,
    last_reload_unix_secs: Option<u64>,
}
