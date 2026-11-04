//! Where a request goes: a [`Destination`] becomes a connectable Pingora peer.
//!
//! The module is deliberately split into the three steps that can fail differently — refusing a
//! scheme this build cannot speak, resolving a name, and building the peer — so each has one place
//! of its own and the pure steps stay testable without a resolver or a socket:
//!
//! - [`Destination`] is the **seam**: whoever understood a route's target (this crate for a document
//!   `upstream`, `nico-ai` for a provider's `base_url`) hands over scheme, host and port, and the
//!   data plane owns everything about *reaching* it — budget, TLS policy, `Host` rewriting. It is not
//!   `nico_core::Upstream`: that type is a document entry a filter may not construct, while a
//!   destination is a per-request decision that any target owner may produce.
//! - [`peer`] composes [`refuse_unavailable_tls`], [`resolve`] and [`build`]. Resolution is the only
//!   I/O step and is asynchronous on purpose: a blocking lookup on a worker thread would stall every
//!   request sharing that runtime, and the default runtime here is one thread per service.
//!
//! Two facts about this layer are easy to get wrong later, so they are stated here:
//!
//! - **ALPN is HTTP/1 by default.** Pingora's `PeerOptions::new()` sets `ALPN::H1`. A gRPC route
//!   must ask for `ALPN::H2` explicitly; this milestone's schema has no gRPC target kind, so the
//!   default is the honest value rather than an unused switch.
//! - **Pingora's connection pool is process-wide**, shared by every request served by one service
//!   instance — not per worker. An HTTP/1 connection carries one request at a time, so the pool
//!   holds roughly as many connections as there are concurrent upstream requests; that is a
//!   capacity fact, not a per-worker tuning knob.
//! - **No TLS provider is compiled**, so upstream TLS is refused rather than faked. Pingora's
//!   no-provider backend is a *pass-through* stub (`protocols/tls/noop_tls`): a peer built with
//!   `tls: true` would not encrypt anything, it would send plaintext to a port the document calls
//!   `https`. Failing is the only honest answer until [`TLS_BACKEND_ENABLED`] flips.

use std::{io, net::SocketAddr, time::Duration};

use nico_core::{Scheme, Upstream};
use pingora::upstreams::peer::HttpPeer;

use crate::error::HttpError;

/// Whether this build can actually speak TLS to an upstream.
///
/// `Cargo.toml` enables pingora without a TLS provider (`boringssl`, `rustls` or `openssl`), so
/// `HttpPeer { tls: true, .. }` is not a TLS connection in this build: pingora's no-op provider
/// returns a pass-through stream. Sending plaintext to a port the document declares `https` would
/// be a silent downgrade, so [`peer`] refuses instead.
///
/// Enabling a provider is one Cargo feature plus flipping this constant — and the `https` mock
/// upstream test in `tests/` then has to start a real TLS listener. Until that task exists, the
/// value stays `false` so the refusal is a compile-time fact rather than a runtime surprise.
pub(crate) const TLS_BACKEND_ENABLED: bool = false;

/// Where one request goes: a scheme, a host and a port.
///
/// This is the data plane's only input for reaching a peer, and it is deliberately constructible
/// outside this crate: a filter that owns a target kind this crate does not understand (a provider
/// named by a route) resolves it and stores the destination in the [`HttpContext`], so provider
/// semantics stay in `nico-ai` while TLS policy, budgets and header rewriting stay here.
///
/// The caller that resolves a target is also the one that validates it — a provider whose
/// `base_url` yields an empty host fails when the peer is connected, exactly like an unreachable
/// upstream, so there is no second error to invent here.
///
/// [`HttpContext`]: crate::HttpContext
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    scheme: Scheme,
    host: String,
    port: u16,
}

impl Destination {
    /// Builds a destination from the three values a target owner knows.
    #[must_use]
    pub fn new(scheme: Scheme, host: impl Into<String>, port: u16) -> Self {
        Self {
            scheme,
            host: host.into(),
            port,
        }
    }

    /// Returns the transport scheme this destination must be reached with.
    #[must_use]
    pub const fn scheme(&self) -> Scheme {
        self.scheme
    }

    /// Returns the destination host, for example `127.0.0.1` or `api.openai.com`.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Returns the destination port.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }
}

impl From<&Upstream> for Destination {
    /// Reads a document upstream as the destination it describes.
    ///
    /// The upstream's name is dropped on purpose: it is a configuration reference used for logging
    /// and for the route lookup, not something the connection needs.
    fn from(upstream: &Upstream) -> Self {
        Self::new(upstream.scheme(), upstream.host(), upstream.port())
    }
}

/// Builds the peer for a destination, bounded by `budget` for both connecting and reading.
///
/// The budget is the request's remaining deadline, so a slow upstream cannot pin resources past it.
/// Callers that have no deadline left must not call this: an expired budget belongs in
/// [`HttpError::DeadlineExceeded`].
///
/// The scheme is checked **before** the lookup: an `https` upstream this build cannot speak to is
/// refused without spending a resolver query on it.
///
/// # Errors
/// Returns [`HttpError::UpstreamFailed`] when the upstream asks for TLS while
/// [`TLS_BACKEND_ENABLED`] is `false`, and when the host does not resolve to a socket address.
/// Both causes are kept as the error's source for logging; the caller-facing response never
/// repeats them.
pub(crate) async fn peer(
    destination: &Destination,
    budget: Duration,
) -> Result<Box<HttpPeer>, HttpError> {
    refuse_unavailable_tls(destination)?;
    let address = resolve(destination).await?;
    Ok(build(destination, address, budget))
}

/// Returns the `Host` header value to send upstream: the host, plus the port when it is not the
/// scheme's default.
#[must_use]
pub(crate) fn host_header(destination: &Destination) -> String {
    if is_default_port(destination.scheme(), destination.port()) {
        destination.host().to_owned()
    } else {
        format!("{}:{}", destination.host(), destination.port())
    }
}

/// Refuses a destination whose scheme this build cannot honour.
///
/// See [`TLS_BACKEND_ENABLED`]: without a provider, `tls: true` is a pass-through stub, so
/// connecting would send plaintext to a port the configuration calls secure.
fn refuse_unavailable_tls(destination: &Destination) -> Result<(), HttpError> {
    if destination.scheme() == Scheme::Https && !TLS_BACKEND_ENABLED {
        return Err(HttpError::UpstreamFailed {
            source: Box::new(io::Error::other(format!(
                "upstream `{}:{}` is https but this build has no TLS provider",
                destination.host(),
                destination.port()
            ))),
        });
    }
    Ok(())
}

/// Resolves the destination's `host:port` to one socket address.
///
/// Pingora's peer holds a single address, so the first resolved one is used — the same choice its
/// own constructor makes. A name is looked up on Tokio's blocking pool, so a slow resolver consumes
/// its own task rather than the worker thread every other request shares.
async fn resolve(destination: &Destination) -> Result<SocketAddr, HttpError> {
    tokio::net::lookup_host((destination.host(), destination.port()))
        .await
        .map_err(upstream_failure)?
        .next()
        .ok_or_else(|| upstream_failure(io::Error::from(io::ErrorKind::AddrNotAvailable)))
}

/// Builds the peer from an address that has already been resolved.
fn build(destination: &Destination, address: SocketAddr, budget: Duration) -> Box<HttpPeer> {
    // Cleartext upstreams still carry the host as SNI: `Host` rewriting and TLS verification both
    // read it, and a peer built for `https` needs it before the handshake.
    let mut peer = HttpPeer::new(
        address,
        destination.scheme() == Scheme::Https,
        destination.host().to_owned(),
    );
    peer.options.connection_timeout = Some(budget);
    peer.options.read_timeout = Some(budget);
    Box::new(peer)
}

/// Wraps a resolution failure as the data plane's upstream failure.
fn upstream_failure(source: io::Error) -> HttpError {
    HttpError::UpstreamFailed {
        source: Box::new(source),
    }
}

/// Reports whether `port` is the one a client assumes when `Host` omits it.
///
/// An unknown scheme's default port is unknown, so its port is written out: an explicit port is
/// always correct, while omitting one is a guess.
fn is_default_port(scheme: Scheme, port: u16) -> bool {
    match scheme {
        Scheme::Http => port == 80,
        Scheme::Https => port == 443,
        _ => false,
    }
}

#[cfg(test)]
mod tests;
