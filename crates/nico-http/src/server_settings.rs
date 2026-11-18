//! The process-level runtime settings the local bootstrap file owns.
//!
//! A worker-thread count and the two shutdown windows describe the *runtime* the data plane runs
//! on, not the protocol it serves, so they are read once from the bootstrap file and never from a
//! configuration source. This module owns their defaults and their accepted ranges, which is why a
//! [`ServerSettings`] can only be built by [`ServerSettings::resolve`] or [`ServerSettings::new`]:
//! every field in an instance was either omitted and defaulted, or written and accepted as written.
//!
//! The values are the only source of the numbers the startup diagnostics quote: an error message is
//! built from the constant the check applied, never from a second copy of it.

use std::borrow::Cow;

/// Default `threads`: Pingora's own default for a service runtime.
///
/// It is not zero: `ServerConf::threads` is handed straight to Tokio's `worker_threads`, which
/// panics on zero, so "let Pingora decide" is expressed by keeping its default, not by writing a
/// zero through.
const DEFAULT_THREADS: usize = 1;

/// Default `grace_period_ms`: how long in-flight requests may finish after shutdown begins.
const DEFAULT_GRACE_PERIOD_MS: u64 = 5_000;

/// Default `graceful_shutdown_ms`: the upper bound on draining once the grace period has elapsed.
const DEFAULT_GRACEFUL_SHUTDOWN_MS: u64 = 30_000;

/// Smallest accepted `threads` value.
const MIN_THREADS: usize = 1;

/// Largest accepted `threads` value.
const MAX_THREADS: usize = 256;

/// Smallest accepted value for either drain window, in milliseconds.
const MIN_WINDOW_MS: u64 = 1;

/// Largest accepted value for either drain window, in milliseconds.
const MAX_WINDOW_MS: u64 = 300_000;

/// Validated runtime settings for the HTTP data plane: worker threads and the shutdown windows.
///
/// Fields are private and the constructors are the only ways to build one, because an unvalidated
/// instance would let a bootstrap typo reach Pingora's `ServerConf` as a drain window of zero
/// seconds or a runtime Tokio cannot build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSettings {
    threads: usize,
    grace_period_ms: u64,
    graceful_shutdown_ms: u64,
}

impl ServerSettings {
    /// Returns the settings an absent `runtime` section stands for.
    ///
    /// Every value is a constant that [`ServerSettings::resolve`] also accepts, so the defaults and
    /// the validation cannot drift apart.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            threads: DEFAULT_THREADS,
            grace_period_ms: DEFAULT_GRACE_PERIOD_MS,
            graceful_shutdown_ms: DEFAULT_GRACEFUL_SHUTDOWN_MS,
        }
    }

    /// Resolves the `runtime` section, defaulting every omitted field.
    ///
    /// A written field is validated even when it happens to equal the default, so a `0` or an
    /// out-of-range value fails at startup instead of being absorbed by the fallback. The two drain
    /// windows are also checked against each other: the shutdown deadline may never be shorter than
    /// the grace period, because that would cut streaming responses off before they could finish.
    ///
    /// # Errors
    /// Returns [`ServerSettingsError`] naming the field and the rule that rejected it, with the
    /// bound quoted from the constant the check applied.
    pub fn resolve(
        threads: Option<usize>,
        grace_period_ms: Option<u64>,
        graceful_shutdown_ms: Option<u64>,
    ) -> Result<Self, ServerSettingsError> {
        let mut settings = Self::new();

        if let Some(value) = threads {
            if !(MIN_THREADS..=MAX_THREADS).contains(&value) {
                return Err(ServerSettingsError::with_owned_reason(
                    "threads",
                    format!("must be in {MIN_THREADS}..={MAX_THREADS} (found {value})"),
                ));
            }
            settings.threads = value;
        }
        if let Some(value) = grace_period_ms {
            settings.grace_period_ms = window("grace_period_ms", value)?;
        }
        if let Some(value) = graceful_shutdown_ms {
            settings.graceful_shutdown_ms = window("graceful_shutdown_ms", value)?;
        }
        if settings.graceful_shutdown_ms < settings.grace_period_ms {
            return Err(ServerSettingsError::with_owned_reason(
                "graceful_shutdown_ms",
                format!(
                    "must be at least `grace_period_ms` ({} ms)",
                    settings.grace_period_ms
                ),
            ));
        }

        Ok(settings)
    }

    /// Returns how many worker threads each service gets.
    ///
    /// It is always at least one: Pingora passes the value to Tokio's `worker_threads`, which
    /// panics on zero.
    #[must_use]
    pub const fn threads(&self) -> usize {
        self.threads
    }

    /// Returns how long, in milliseconds, in-flight requests may finish once shutdown has begun.
    #[must_use]
    pub const fn grace_period_ms(&self) -> u64 {
        self.grace_period_ms
    }

    /// Returns, in milliseconds, the upper bound on draining once the grace period has elapsed.
    #[must_use]
    pub const fn graceful_shutdown_ms(&self) -> u64 {
        self.graceful_shutdown_ms
    }
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self::new()
    }
}

/// A bootstrap field that is syntactically valid but outside the range this build accepts.
///
/// Both parts borrow for a fixed rule and own only when they quote a limit, so a message can never
/// drift from the constant the check applied.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("runtime setting `{field}` is invalid: {reason}")]
pub struct ServerSettingsError {
    field: Cow<'static, str>,
    reason: Cow<'static, str>,
}

impl ServerSettingsError {
    /// Builds an error whose reason quotes the bound that rejected the value.
    ///
    /// Every rule in this module quotes a range or the other window's value, so there is no
    /// fixed-sentence constructor: a reason here is always derived from a constant.
    fn with_owned_reason(field: &'static str, reason: String) -> Self {
        Self {
            field: field.into(),
            reason: reason.into(),
        }
    }

    /// Returns the field name, without the `runtime.` prefix the bootstrap section adds.
    #[must_use]
    pub fn field(&self) -> &str {
        &self.field
    }

    /// Returns why the value was rejected, derived from the rule that rejected it.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// Checks one drain window against the accepted range, in milliseconds.
fn window(field: &'static str, value: u64) -> Result<u64, ServerSettingsError> {
    if !(MIN_WINDOW_MS..=MAX_WINDOW_MS).contains(&value) {
        return Err(ServerSettingsError::with_owned_reason(
            field,
            format!("must be in {MIN_WINDOW_MS}..={MAX_WINDOW_MS} (found {value})"),
        ));
    }
    Ok(value)
}

#[cfg(test)]
mod tests;
