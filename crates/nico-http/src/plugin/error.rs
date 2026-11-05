//! How a plugin fails: a refusal, an execution failure, or a broken phase contract.
//!
//! Three variants, because a plugin makes exactly three decisions the data plane must treat
//! differently:
//!
//! - [`PluginError::Refused`] — the caller is not allowed in. It carries a compiled
//!   [`Rejection`](crate::Rejection), so the refusal keeps the stable code, the category and the
//!   presentation an operator configured, and the data plane writes it like any other typed failure.
//! - [`PluginError::Execute`] — the plugin could not do its job. That is this gateway's own defect,
//!   so it becomes a `500` and the log names the plugin.
//! - [`PluginError::PhaseViolation`] — the plugin asked for something its phase does not have (a
//!   request body in `on_request`, a status before a response exists). Also `500`: a document cannot
//!   cause it, so it is a plugin bug rather than the caller's mistake.
//!
//! **This is a request-path error type only.** Everything that can go wrong while a document is
//! *compiled* — an unknown kind, a configuration field that does not exist, a credential reference
//! that cannot be read — is a publication failure owned by the crate that compiles the kind, and it
//! keeps the last known good generation serving. Merging the two vocabularies would let a request
//! inherit an error that only publication can produce, and would hide from the compiler that a
//! request path may not allocate a configuration message.
//!
//! The type deliberately does not name [`HttpError`](crate::HttpError): a plugin must not know the
//! data plane's whole vocabulary, where upstream failures, timeouts and cancellation live. The one
//! conversion lives in [`HttpError`](crate::HttpError), so `(status, code)` is still decided in
//! exactly one place.

use std::{sync::Arc, time::Duration};

use crate::error::Rejection;

/// Why a plugin stage did not return [`Outcome::Continue`](super::Outcome::Continue).
///
/// The set may grow, so matches outside the defining module must name the variants they handle.
#[derive(Debug, thiserror::Error, Clone, PartialEq)]
#[non_exhaustive]
pub enum PluginError {
    /// The plugin refused the request: the caller's credential is missing or not acceptable, its
    /// quota is spent, its request does not satisfy a policy.
    ///
    /// `rejection` is the presentation compiled at publication — status, content type, stable code
    /// and body template. `detail` is a fixed sentence for the log; it must not quote a credential,
    /// and it never reaches the caller.
    #[error("plugin refused the request: {detail}")]
    Refused {
        /// The compiled presentation of this refusal, shared by every request the plugin refuses.
        rejection: Arc<Rejection>,
        /// Why the request was refused, for logs only.
        detail: &'static str,
        /// How long the caller should wait before retrying, when the plugin knows.
        retry_after: Option<Duration>,
    },

    /// The plugin failed to do its job — a malformed credential it was configured to verify, a
    /// template it could not render, an invariant it could not uphold.
    ///
    /// This is a gateway fault: it becomes a `500`, and the log line names the plugin instance.
    #[error("plugin failed to execute: {0}")]
    Execute(&'static str),

    /// The plugin called a context method its phase does not offer.
    ///
    /// A document cannot provoke it, so it is reported as [`Self::Execute`] is — a `500` naming the
    /// plugin — instead of silently doing nothing, which would let a plugin believe a rewrite it did
    /// not perform had taken effect.
    #[error("plugin violated its phase contract: {0}")]
    PhaseViolation(&'static str),
}

/// The severity a plugin attributes to one line it logs.
///
/// A closed set rather than a string: the value crosses a WASM boundary in the next milestone, and a
/// host that had to parse a level name would be the second place that decides what a level means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    /// Diagnostic detail, off unless the operator turned it on.
    Debug,
    /// A normal event worth a line, such as "compiled with three keys".
    Info,
    /// Something surprising that did not stop the request.
    Warn,
    /// A failure the plugin already reported as an error.
    Error,
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use nico_core::ErrorCategory;
    use pingora::http::StatusCode;

    use super::*;
    use crate::error::{BodyTemplate, HttpError};

    fn rejection() -> Arc<Rejection> {
        Arc::new(
            Rejection::new(
                StatusCode::UNAUTHORIZED,
                "auth_invalid",
                "the credential is not acceptable",
                "application/json",
                ErrorCategory::Client,
                BodyTemplate::error_envelope(),
            )
            .expect("the fixture rejection is valid"),
        )
    }

    #[test]
    fn a_refusal_keeps_its_presentation_and_retry_after() {
        let error = HttpError::from(PluginError::Refused {
            rejection: rejection(),
            detail: "the token is not signed by a configured key",
            retry_after: Some(Duration::from_secs(2)),
        });

        let respond = error.respond().expect("a refusal is a response");
        assert_eq!(respond.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(respond.code(), "auth_invalid");
        assert_eq!(respond.retry_after(), Some(Duration::from_secs(2)));
        assert_eq!(error.category(), ErrorCategory::Client);
        assert!(error.to_string().contains("the token is not signed"));
    }

    #[test]
    fn an_execution_failure_is_internal_and_names_the_plugin_layer() {
        let error = HttpError::from(PluginError::Execute("the key set is empty"));

        assert!(matches!(error, HttpError::Internal { .. }), "got {error:?}");
        assert_eq!(
            error.respond().expect("internal errors answer").status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(error.category(), ErrorCategory::Internal);
        assert_eq!(
            error.to_string(),
            "internal data-plane failure",
            "the caller-safe message stays generic"
        );
        let cause = error.source().map(ToString::to_string).unwrap_or_default();
        assert!(
            cause.to_lowercase().contains("plugin"),
            "the cause must make the plugin layer visible: {cause}"
        );
    }

    #[test]
    fn a_phase_violation_is_internal_and_not_attributed_to_the_caller() {
        let error = HttpError::from(PluginError::PhaseViolation(
            "request bodies are only available in on_request_body",
        ));

        assert!(matches!(error, HttpError::Internal { .. }), "got {error:?}");
        assert_eq!(error.category(), ErrorCategory::Internal);
        let source = error.source().map(ToString::to_string).unwrap_or_default();
        assert!(
            source.contains("on_request_body"),
            "the cause must be kept for the log: {source}"
        );
    }
}
