//! Where a request goes: a [`Destination`] becomes a connectable Pingora peer.
//!
//! The module is deliberately split into the steps that can fail differently — resolving a name and
//! building the peer — so each has one place of its own and the pure step stays testable without a
//! resolver or a socket:
//!
//! - [`Destination`] is the **seam**: whoever understood a route's target (this crate for a document
//!   `upstream`, `nico-llm` for a provider's `base_url`) hands over scheme, host and port, and the
//!   data plane owns everything about *reaching* it — budget, TLS policy, `Host` rewriting. It is not
//!   `nico_core::Upstream`: that type is a document entry a filter may not construct, while a
//!   destination is a per-request decision that any target owner may produce.
//! - [`peer`] composes [`resolve`] and [`build`]. Resolution is the only I/O step and is asynchronous
//!   on purpose: a blocking lookup on a worker thread would stall every request sharing that runtime,
//!   and the default runtime here is one thread per service.
//!
//! Three facts about this layer are easy to get wrong later, so they are stated here:
//!
//! - **ALPN is HTTP/1 by default.** Pingora's `PeerOptions::new()` sets `ALPN::H1`. A gRPC route
//!   must ask for `ALPN::H2` explicitly; this milestone's schema has no gRPC target kind, so the
//!   default is the honest value rather than an unused switch.
//! - **Pingora's connection pool is process-wide**, shared by every request served by one service
//!   instance — not per worker. An HTTP/1 connection carries one request at a time, so the pool
//!   holds roughly as many connections as there are concurrent upstream requests; that is a
//!   capacity fact, not a per-worker tuning knob.
//! - **TLS is a build fact, not a per-request decision.** This crate enables pingora's `rustls`
//!   backend, so an `https` destination is a real TLS connection; *which roots* are trusted is
//!   [`tls`]'s business and is fixed when the service is built, never here. A peer only declares
//!   that its target is `https` and carries the SNI host; nothing on this path can downgrade the
//!   connection to plaintext, because the backend is compiled in.

use std::{io, net::SocketAddr, time::Duration};

use nico_core::{Scheme, Upstream};
use pingora::upstreams::peer::HttpPeer;

use crate::error::HttpError;

pub(crate) mod tls;

/// Where one request goes: a scheme, a host and a port.
///
/// This is the data plane's only input for reaching a peer, and it is deliberately constructible
/// outside this crate: a filter that owns a target kind this crate does not understand (a provider
/// named by a route) resolves it and stores the destination in the [`HttpContext`], so provider
/// semantics stay in `nico-llm` while TLS policy, budgets and header rewriting stay here.
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
/// Which roots this peer's handshake is verified against is not decided here: it is a
/// process-level fact applied to Pingora's server configuration once, when the service is built
/// (see [`tls`]). The scheme decides only *whether* the connection is a TLS connection.
///
/// # Errors
/// Returns [`HttpError::UpstreamFailed`] when the host does not resolve to a socket address. The
/// cause is kept as the error's source for logging; the caller-facing response never repeats it.
pub(crate) async fn peer(
    destination: &Destination,
    budget: Duration,
) -> Result<Box<HttpPeer>, HttpError> {
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
