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
//! - [`pools`] answers *which* member of a multi-address upstream a request connects to. The strategy
//!   itself comes from `nico-lb`, which owns the selection contract and each strategy's state; the
//!   pool here owns what the gateway observed about the members, so a reload replaces the members
//!   without resetting either.
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

use std::{io, net::SocketAddr, sync::Arc, time::Duration};

use nico_core::Scheme;
use pingora::upstreams::peer::HttpPeer;

use crate::error::HttpError;

pub(crate) mod dns;
pub(crate) mod pools;
pub(crate) mod tls;

/// Where one request goes: a transport, the address to connect to, and the name the peer answers to.
///
/// This is the data plane's only input for reaching a peer, and it is deliberately constructible
/// outside this crate: a filter that owns a target kind this crate does not understand (a provider
/// named by a route) resolves it and stores the destination in the [`HttpContext`], so provider
/// semantics stay in `nico-llm` while TLS policy, budgets and header rewriting stay here.
///
/// `host` and `authority` are two different questions:
///
/// - `host` is the **connection target**: what the resolver looks up and what the socket is opened to.
/// - `authority` is the **name** the peer is addressed by: the `Host` header and the TLS server name.
///
/// They are equal for every destination a target owner builds by hand, which is why
/// [`Destination::new`] sets both. They differ once an upstream has several members: one certificate
/// names a service, and the service may run on many machines, so a request connects to `10.0.0.7` and
/// still says `api.example.test`. Writing the machine into `Host` would address a virtual host nobody
/// declared; using it for SNI would fail a certificate that was issued for the service.
///
/// The caller that resolves a target is also the one that validates it — a provider whose
/// `base_url` yields an empty host fails when the peer is connected, exactly like an unreachable
/// upstream, so there is no second error to invent here.
///
/// [`HttpContext`]: crate::HttpContext
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    scheme: Scheme,
    host: Arc<str>,
    port: u16,
    authority: Arc<str>,
}

impl Destination {
    /// Builds a destination from the three values a target owner knows.
    ///
    /// The authority starts equal to `host`, because a target owner that names one address has one
    /// name for it. Only the upstream selector separates the two.
    #[must_use]
    pub fn new(scheme: Scheme, host: impl Into<Arc<str>>, port: u16) -> Self {
        let host = host.into();
        Self {
            scheme,
            authority: Arc::clone(&host),
            host,
            port,
        }
    }

    /// Replaces the name the peer is addressed by, leaving the connection target alone.
    ///
    /// The one caller is the upstream selector: it chose a member's address and keeps the upstream's
    /// logical host for `Host` and SNI. A policy building a destination directly never needs it.
    #[must_use]
    pub fn with_authority(mut self, authority: impl Into<Arc<str>>) -> Self {
        self.authority = authority.into();
        self
    }

    /// Returns the transport scheme this destination must be reached with.
    #[must_use]
    pub const fn scheme(&self) -> Scheme {
        self.scheme
    }

    /// Returns the connection target, for example `127.0.0.1` or `10.0.0.7`.
    ///
    /// This is what the resolver looks up. It is **not** what the upstream is addressed by: see
    /// [`Destination::authority`].
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Returns the destination port, which is the port the connection is opened to.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Returns the name the peer is addressed by: the `Host` header and the TLS server name.
    ///
    /// For a destination a target owner built by hand this equals [`Destination::host`]. For a member
    /// of a multi-address upstream it is the upstream's logical host, so a certificate issued for the
    /// service verifies no matter which machine served the request.
    #[must_use]
    pub fn authority(&self) -> &str {
        &self.authority
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
    read_budget: Option<Duration>,
) -> Result<Box<HttpPeer>, HttpError> {
    let address = resolve(destination).await?;
    Ok(build(destination, address, budget, read_budget))
}

/// Returns the `Host` header value to send upstream: the authority, plus the port when it is not the
/// scheme's default.
///
/// The port is the one the connection uses. A member that declares its own port therefore still says
/// the logical host, and the pair reads as the service the caller addressed — which is what a virtual
/// host matches on.
#[must_use]
pub(crate) fn host_header(destination: &Destination) -> String {
    if is_default_port(destination.scheme(), destination.port()) {
        destination.authority().to_owned()
    } else {
        format!("{}:{}", destination.authority(), destination.port())
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
///
/// The server name is the destination's authority, never its connection target: a certificate names
/// the service, and a member's address is a machine. A cleartext peer carries it too — `Host`
/// rewriting and TLS verification both read it — so a later upgrade of the same route to `https`
/// verifies against the name the operator wrote.
fn build(
    destination: &Destination,
    address: SocketAddr,
    budget: Duration,
    read_budget: Option<Duration>,
) -> Box<HttpPeer> {
    let mut peer = HttpPeer::new(
        address,
        destination.scheme() == Scheme::Https,
        destination.authority().to_owned(),
    );
    peer.options.connection_timeout = Some(budget);
    // The read budget is the route's first-byte budget when it states one, and the request's own
    // remaining time otherwise. It is a *per-read* bound — pingora sets it once on the session and
    // applies it to every read, header and body alike — so a streamed answer's budget also bounds the
    // gap between two chunks. That is stated in the route's documentation rather than hidden: a
    // gateway that silently treated "first byte" and "any byte" as the same thing would be lying
    // about the field it accepted.
    // `None` is a deliberate value, not an omission: a request-buffering route must not run a
    // read timeout while the gateway is still collecting the client's body — the upstream is
    // waiting for us, and a slow upload would die as a 504 before its 408 could fire. The
    // deadline check on every collected chunk is what ends such a request (`408`).
    peer.options.read_timeout = read_budget.map(|read| read.min(budget));
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
