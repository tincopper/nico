//! The process-level HTTP settings the local bootstrap file owns.
//!
//! A listening address, plaintext HTTP/2, an optional inbound TLS identity, drain windows, a thread
//! count and the upstream trust roots describe the *process*, not the configuration a control plane
//! pushes, so they are read once from the bootstrap file and never from a configuration source. This module owns their defaults and
//! their accepted ranges, which is why [`HttpSettings`] can only be built by [`HttpSettings::resolve`]:
//! every field in an instance was either omitted and defaulted, or written and accepted as written.
//!
//! The values are the only source of the numbers the startup diagnostics quote: an error message is
//! built from the constant the check applied, never from a second copy of it.

use std::{borrow::Cow, net::SocketAddr, time::Duration};

use crate::{inbound::InboundTls, upstream::tls::TlsPolicy};

/// Address the data plane listens on when the bootstrap file names none.
const DEFAULT_LISTEN: &str = "0.0.0.0:8080";

/// Default `grace_period_ms`: how long in-flight requests may finish after shutdown begins.
const DEFAULT_GRACE_PERIOD_MS: u64 = 5_000;

/// Default `graceful_shutdown_ms`: the upper bound on draining once the grace period has elapsed.
const DEFAULT_GRACEFUL_SHUTDOWN_MS: u64 = 30_000;

/// Largest accepted value for either drain window, in milliseconds.
const MAX_WINDOW_MS: u64 = 300_000;

/// Default `threads`: Pingora's own default for a service runtime.
///
/// It is not zero: `ServerConf::threads` is handed straight to Tokio's `worker_threads`, which
/// panics on zero, so "let Pingora decide" is expressed by keeping its default, not by writing a
/// zero through.
const DEFAULT_THREADS: usize = 1;

/// Smallest accepted `threads` value.
const MIN_THREADS: usize = 1;

/// Largest accepted `threads` value.
const MAX_THREADS: usize = 256;

/// Validated process-level settings for the HTTP data plane.
///
/// Fields are private and there is no public constructor besides [`HttpSettings::resolve`], because
/// an unvalidated instance would let a bootstrap typo reach Pingora's `ServerConf` as a drain
/// window of zero seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpSettings {
    listen: String,
    h2c: bool,
    grace_period: Duration,
    graceful_shutdown: Duration,
    threads: usize,
    tls: TlsPolicy,
    inbound_tls: Option<InboundTls>,
}

impl HttpSettings {
    /// Resolves the `http` section, defaulting every omitted field.
    ///
    /// A written field is validated even when it happens to equal the default, so a `0` or an
    /// out-of-range value fails at startup instead of being absorbed by the fallback. The two drain
    /// windows are also checked against each other: the shutdown deadline may never be shorter than
    /// the grace period, because that would cut streaming responses off before they could finish.
    ///
    /// `tls` is passed in rather than defaulted here: building one means reading a file, and reading
    /// files is not this module's job. The bootstrap section is the one caller, and it validates the
    /// bundle before constructing the policy, so the value that reaches the server configuration was
    /// checked — a policy records the fact, and this module does not re-check it.
    ///
    /// # Errors
    /// Returns [`HttpSettingsError`] naming the field and the rule that rejected it, with the bound
    /// quoted from the constant the check applied.
    pub fn resolve(
        listen: Option<&str>,
        h2c: Option<bool>,
        grace_period_ms: Option<u64>,
        graceful_shutdown_ms: Option<u64>,
        threads: Option<usize>,
        tls: TlsPolicy,
    ) -> Result<Self, HttpSettingsError> {
        Self::resolve_with_inbound_tls(
            listen,
            h2c,
            grace_period_ms,
            graceful_shutdown_ms,
            threads,
            tls,
            None,
        )
    }

    /// Resolves the section together with the listener's TLS identity.
    ///
    /// The identity is a pair of paths the bootstrap section validated; it is not read here. A
    /// configured identity and `h2c` are mutually exclusive: one listener cannot be both TLS and
    /// cleartext HTTP/2, and silently ignoring one of the two would serve a transport the operator
    /// did not ask for.
    ///
    /// # Errors
    /// Returns [`HttpSettingsError`] naming the field and the rule that rejected it. The checks are
    /// the same as [`HttpSettings::resolve`], plus the `h2c`/TLS conflict.
    pub fn resolve_with_inbound_tls(
        listen: Option<&str>,
        h2c: Option<bool>,
        grace_period_ms: Option<u64>,
        graceful_shutdown_ms: Option<u64>,
        threads: Option<usize>,
        tls: TlsPolicy,
        inbound_tls: Option<InboundTls>,
    ) -> Result<Self, HttpSettingsError> {
        let mut settings = Self {
            tls,
            inbound_tls,
            ..Self::default()
        };

        if let Some(listen) = listen {
            settings.listen = listen.to_owned();
        }
        validate_listen(&settings.listen)?;

        if let Some(h2c) = h2c {
            settings.h2c = h2c;
        }
        if settings.h2c && settings.inbound_tls.is_some() {
            return Err(HttpSettingsError::new(
                "h2c",
                "cannot be enabled together with `http.tls`: one listener has one transport",
            ));
        }
        if let Some(value) = grace_period_ms {
            settings.grace_period = window("grace_period_ms", value)?;
        }
        if let Some(value) = graceful_shutdown_ms {
            settings.graceful_shutdown = window("graceful_shutdown_ms", value)?;
        }
        if settings.graceful_shutdown < settings.grace_period {
            return Err(HttpSettingsError::with_owned_reason(
                "graceful_shutdown_ms",
                format!(
                    "must be at least `grace_period_ms` ({} ms)",
                    settings.grace_period.as_millis()
                ),
            ));
        }
        if let Some(value) = threads {
            if !(MIN_THREADS..=MAX_THREADS).contains(&value) {
                return Err(HttpSettingsError::with_owned_reason(
                    "threads",
                    format!("must be in {MIN_THREADS}..={MAX_THREADS} (found {value})"),
                ));
            }
            settings.threads = value;
        }

        Ok(settings)
    }

    /// Returns the address the data plane listens on.
    #[must_use]
    pub fn listen(&self) -> &str {
        &self.listen
    }

    /// Returns whether cleartext HTTP/2 (h2c) is accepted on the listener.
    #[must_use]
    pub const fn h2c(&self) -> bool {
        self.h2c
    }

    /// Returns how long in-flight requests may finish once shutdown has begun.
    #[must_use]
    pub const fn grace_period(&self) -> Duration {
        self.grace_period
    }

    /// Returns the upper bound on draining once the grace period has elapsed.
    #[must_use]
    pub const fn graceful_shutdown(&self) -> Duration {
        self.graceful_shutdown
    }

    /// Returns how many worker threads each service gets.
    ///
    /// It is always at least one: Pingora passes the value to Tokio's `worker_threads`, which
    /// panics on zero.
    #[must_use]
    pub const fn threads(&self) -> usize {
        self.threads
    }

    /// Returns the trust roots an upstream TLS handshake is verified against.
    ///
    /// It is a startup fact, not a document field: Pingora builds its connector from this value
    /// once, so changing it takes a restart rather than a reload.
    #[must_use]
    pub const fn tls(&self) -> &TlsPolicy {
        &self.tls
    }

    /// Returns the listener's TLS identity, when this process terminates TLS.
    ///
    /// The value is a pair of paths; the service reads and validates them when it builds the
    /// listener. `None` means plaintext TCP, exactly as before this setting existed.
    #[must_use]
    pub const fn inbound_tls(&self) -> Option<&InboundTls> {
        self.inbound_tls.as_ref()
    }
}

impl Default for HttpSettings {
    /// Returns the settings an omitted `http` section stands for.
    ///
    /// Every field is a constant that [`HttpSettings::resolve`] also accepts, so the defaults and
    /// the validation cannot drift apart: `resolve` starts from this value and validates it.
    fn default() -> Self {
        Self {
            listen: DEFAULT_LISTEN.to_owned(),
            h2c: false,
            grace_period: Duration::from_millis(DEFAULT_GRACE_PERIOD_MS),
            graceful_shutdown: Duration::from_millis(DEFAULT_GRACEFUL_SHUTDOWN_MS),
            threads: DEFAULT_THREADS,
            tls: TlsPolicy::default(),
            inbound_tls: None,
        }
    }
}

/// A bootstrap field that is syntactically valid but outside the range this build accepts.
///
/// Both parts borrow for a fixed rule and own only when they quote a limit, so a message can never
/// drift from the constant the check applied.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("http setting `{field}` is invalid: {reason}")]
pub struct HttpSettingsError {
    field: Cow<'static, str>,
    reason: Cow<'static, str>,
}

impl HttpSettingsError {
    /// Builds an error for a field and a fixed rule.
    fn new(field: &'static str, reason: &'static str) -> Self {
        Self {
            field: field.into(),
            reason: reason.into(),
        }
    }

    /// Builds an error whose reason quotes the bound that rejected the value.
    fn with_owned_reason(field: &'static str, reason: String) -> Self {
        Self {
            field: field.into(),
            reason: reason.into(),
        }
    }

    /// Returns the field name, without the `http.` prefix the bootstrap section adds.
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

/// Turns a window in milliseconds into a duration, rejecting one outside the accepted range.
fn window(field: &'static str, value: u64) -> Result<Duration, HttpSettingsError> {
    if !(1..=MAX_WINDOW_MS).contains(&value) {
        return Err(HttpSettingsError::with_owned_reason(
            field,
            format!("must be in 1..={MAX_WINDOW_MS} (found {value})"),
        ));
    }
    Ok(Duration::from_millis(value))
}

/// Checks that `listen` names a bindable address with a non-zero port.
///
/// This is the single definition of "what counts as a legal address" for a listener: the data
/// plane's own `http.listen` and the metrics listener's `metrics.listen` are both checked here, so
/// the two surfaces cannot accept different shapes.
///
/// An IP literal is parsed for real; a name is only checked for shape, because resolving it here
/// would make startup depend on DNS for an address the operator already knows is local. A bind
/// failure then surfaces when the listener is opened, which is where Pingora reports it.
///
/// # Errors
/// Returns an [`HttpSettingsError`] whose field is `listen` when the value is not an `ip:port` or
/// `host:port` address, or when the port is zero.
pub fn validate_listen(listen: &str) -> Result<(), HttpSettingsError> {
    let port = match listen.parse::<SocketAddr>() {
        Ok(address) => address.port(),
        Err(_error) => match listen.rsplit_once(':') {
            Some((host, port)) if !host.is_empty() => {
                port.parse::<u16>().map_err(|_error| bad_listen())?
            }
            _ => return Err(bad_listen()),
        },
    };
    if port == 0 {
        return Err(HttpSettingsError::new(
            "listen",
            "must name a port other than zero",
        ));
    }
    Ok(())
}

/// The error for an address that is not `host:port`.
fn bad_listen() -> HttpSettingsError {
    HttpSettingsError::new("listen", "must be an `ip:port` or `host:port` address")
}

#[cfg(test)]
mod tests;
