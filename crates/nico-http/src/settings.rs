//! The validated `http` settings the local bootstrap file owns.
//!
//! A listener is one address and the transport it serves: a plaintext entry, or a TLS entry whose
//! identity the bootstrap already validated. The listeners, plaintext HTTP/2 and the upstream trust
//! roots describe *this process* — protocol facts, not configuration a control plane pushes — so
//! they are read once from the bootstrap file and never from a configuration source. This module
//! owns their accepted shapes, which is why an [`HttpSettings`] can only be built by
//! [`HttpSettings::resolve`]: every listener in an instance names a legal address that no other
//! listener repeats, and `h2c` names a listener it actually applies to.
//!
//! The values are the only source of the numbers the startup diagnostics quote: an error message is
//! built from the constant the check applied, never from a second copy of it.

use std::{
    borrow::Cow,
    net::{SocketAddr, ToSocketAddrs},
};

use crate::{inbound::InboundTls, upstream::tls::TlsPolicy};

/// The fixed reason an address is not a usable listener address.
///
/// Both the `http.listeners` entries and the metrics listener are checked against the same rule, so
/// the sentence lives here once and every error borrows it.
const BAD_LISTEN_REASON: &str = "must be an `ip:port` or `host:port` address";

/// One listener of the `http` section: an address, and the transport it serves.
///
/// The variant **is** the transport decision, so a TLS entry structurally cannot carry an `h2c`
/// flag and a plaintext entry structurally cannot carry a certificate: neither combination can be
/// constructed, let alone silently resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listener {
    /// A plaintext TCP listener: HTTP/1.1 and, when `h2c` is set, cleartext HTTP/2.
    Plain {
        /// The address this listener binds.
        listen: String,
    },
    /// A TLS listener: HTTP over the certificate and key it presents.
    Tls {
        /// The address this listener binds.
        listen: String,
        /// The identity this listener presents; a pair of paths the bootstrap validated.
        tls: InboundTls,
    },
}

impl Listener {
    /// Returns the address this entry binds, whatever transport it serves.
    fn listen(&self) -> &str {
        match self {
            Self::Plain { listen } | Self::Tls { listen, .. } => listen,
        }
    }
}

/// Validated settings for the HTTP data plane's listeners.
///
/// Fields are private and there is no public constructor besides [`HttpSettings::resolve`], because
/// an unvalidated instance would let a bootstrap typo reach Pingora as two listeners bound to one
/// address, or as a `h2c` flag with no listener to apply it to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpSettings {
    listeners: Vec<Listener>,
    h2c: bool,
    tls: TlsPolicy,
}

impl HttpSettings {
    /// Resolves the `http` section from its listeners.
    ///
    /// The section must name at least one listener, and the entries must resolve to pairwise
    /// different addresses: two listeners that name one socket would fight over a bind Pingora can
    /// only lose. Addresses are compared **after** resolution, so two spellings of one socket
    /// (`127.0.0.1:9000` and `localhost:9000`) collide just like a literal repeat does; an entry
    /// whose address does not resolve is rejected with the same shape error every listener address
    /// goes through.
    ///
    /// `h2c` is a section-level fact that applies to the plaintext listeners: a TLS listener
    /// negotiates HTTP/2 through ALPN and never accepts cleartext HTTP/2, so `h2c: true` with no
    /// plaintext entry would state a knob with nothing to apply it to. Together with TLS entries it
    /// is fine — one listener per transport, each transport served where it is declared.
    ///
    /// `tls` is passed in rather than defaulted here: building one means reading a file, and reading
    /// files is not this module's job. The bootstrap section is the one caller, and it validates the
    /// bundle before constructing the policy, so the value that reaches the server configuration was
    /// checked — a policy records the fact, and this module does not re-check it.
    ///
    /// # Errors
    /// Returns [`HttpSettingsError`] naming the field and the rule that rejected it: an empty
    /// listener list, an unusable or repeated address (field `listeners`), or `h2c` without a
    /// plaintext listener (field `h2c`).
    pub fn resolve(
        listeners: Vec<Listener>,
        h2c: Option<bool>,
        tls: TlsPolicy,
    ) -> Result<Self, HttpSettingsError> {
        if listeners.is_empty() {
            return Err(HttpSettingsError::new(
                "listeners",
                "must name at least one listener",
            ));
        }
        pairwise_distinct(&listeners)?;
        let settings = Self {
            listeners,
            h2c: h2c.unwrap_or(false),
            tls,
        };
        // The plaintext subset is read through the accessor rather than re-matched here, so "which
        // listeners does `h2c` apply to" has one definition for the rule and for every caller.
        if settings.h2c && settings.plain_addresses().is_empty() {
            return Err(HttpSettingsError::new(
                "h2c",
                "requires a plaintext listener: every configured listener terminates TLS",
            ));
        }

        Ok(settings)
    }

    /// Returns the listeners the data plane binds, in the order the section named them.
    #[must_use]
    pub fn listeners(&self) -> &[Listener] {
        &self.listeners
    }

    /// Returns the address of every listener, in the order the section named them.
    ///
    /// The self-reporting listener is checked against this set, so a `metrics.listen` that names any
    /// listener the data plane binds — plaintext or TLS — is refused before either is opened.
    #[must_use]
    pub fn listener_addresses(&self) -> Vec<&str> {
        self.listeners.iter().map(Listener::listen).collect()
    }

    /// Returns the addresses of the plaintext listeners, in the order the section named them.
    ///
    /// This is the subset `h2c` applies to: a TLS listener negotiates HTTP/2 through ALPN and never
    /// accepts cleartext HTTP/2, so a self-reporting listener that names a TLS address collides with
    /// [`HttpSettings::listener_addresses`], not with this set.
    #[must_use]
    pub fn plain_addresses(&self) -> Vec<&str> {
        self.listeners
            .iter()
            .filter_map(|listener| match listener {
                Listener::Plain { listen } => Some(listen.as_str()),
                Listener::Tls { .. } => None,
            })
            .collect()
    }

    /// Returns whether cleartext HTTP/2 (h2c) is accepted on the plaintext listeners.
    #[must_use]
    pub const fn h2c(&self) -> bool {
        self.h2c
    }

    /// Returns the trust roots an upstream TLS handshake is verified against.
    ///
    /// It is a startup fact, not a document field: Pingora builds its connector from this value
    /// once, so changing it takes a restart rather than a reload.
    #[must_use]
    pub const fn tls(&self) -> &TlsPolicy {
        &self.tls
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

/// Checks that every entry names a usable address and that no two entries resolve to one socket.
///
/// The shape rule is the same one the metrics listener goes through (`validate_listen`), so the two
/// surfaces cannot accept different shapes; the error is reported against the entries, whose
/// configuration path is `http.listeners`. Resolution is what makes the comparison honest: two
/// spellings of one socket must collide even though their strings differ.
fn pairwise_distinct(listeners: &[Listener]) -> Result<(), HttpSettingsError> {
    let mut resolved: Vec<Vec<SocketAddr>> = Vec::with_capacity(listeners.len());
    for listener in listeners {
        let listen = listener.listen();
        validate_listen(listen).map_err(|error| {
            HttpSettingsError::with_owned_reason("listeners", error.reason().to_owned())
        })?;
        let addrs: Vec<SocketAddr> = listen
            .to_socket_addrs()
            .map_err(|_| HttpSettingsError::new("listeners", BAD_LISTEN_REASON))?
            .collect();
        if addrs.is_empty() {
            return Err(HttpSettingsError::new("listeners", BAD_LISTEN_REASON));
        }
        if resolved
            .iter()
            .any(|previous| previous.iter().any(|address| addrs.contains(address)))
        {
            return Err(HttpSettingsError::with_owned_reason(
                "listeners",
                format!("must not repeat `{listen}`: two listeners cannot bind one address"),
            ));
        }
        resolved.push(addrs);
    }
    Ok(())
}

/// Checks that `listen` names a bindable address with a non-zero port.
///
/// This is the single definition of "what counts as a legal address" for a listener: the data
/// plane's `http.listeners` entries and the metrics listener's `metrics.listen` are both checked
/// here, so the two surfaces cannot accept different shapes.
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
    HttpSettingsError::new("listen", BAD_LISTEN_REASON)
}

#[cfg(test)]
mod tests;
