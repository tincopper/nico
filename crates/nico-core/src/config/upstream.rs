//! Plain HTTP upstreams a route can forward to, and the members a request may be sent to.
//!
//! An [`Upstream`] carries two different facts, and keeping them apart is the whole point of this
//! module:
//!
//! - the **logical identity** — `scheme`, `host`, `port` — which is what the `Host` header and the TLS
//!   server name say. A certificate is issued for a name, so it has to be the name, not a machine;
//! - the **members** — at least one [`Endpoint`], each with the `address` a request actually connects
//!   to, its own port and its share of the traffic.
//!
//! A single-address document writes only the identity, and the parser expands it into one member named
//! [`DEFAULT_ENDPOINT_NAME`]. The document layer therefore has no "single instance" branch to keep in
//! step with the multi-member one, and the data plane has one shape to select from.
//!
//! The type deliberately carries no path — the client's path is forwarded as-is by the data plane —
//! and no credential.

use std::sync::Arc;

use serde::Deserialize;

/// The name the single member of a document that writes no `endpoints` is given.
///
/// It is a member name like any other: it appears in logs and metrics exactly where an explicit
/// `endpoints[].name` would, so a deployment that later grows a second member does not change the
/// shape of its observability.
pub const DEFAULT_ENDPOINT_NAME: &str = "default";

/// The weight a member that writes none is given.
pub const DEFAULT_ENDPOINT_WEIGHT: u16 = 1;

/// The consecutive failures a member tolerates when the document writes no `max_fails`.
///
/// Three is the smallest number that is not "eject on one flaky request": a single reset is common
/// enough on a busy machine that ejecting on it would take a healthy member out of rotation, while a
/// member that fails three times in a row has stopped answering.
pub const DEFAULT_MAX_FAILS: u32 = 3;

/// How long an ejected member stays out of rotation when the document writes no
/// `fail_timeout_ms`.
///
/// Thirty seconds is long enough for a restarting process to come back and short enough that an
/// operator who fixed the machine does not have to wait for a timer they cannot see.
pub const DEFAULT_FAIL_TIMEOUT_MS: u32 = 30_000;

/// The failure budget one upstream's members are judged by.
///
/// A member is ejected after `max_fails` consecutive failures and probed again after
/// `fail_timeout_ms`. `max_fails` of `0` disables ejection for the upstream — failures are still
/// counted and an operator still reads them from the record, but no member is ever taken out of
/// rotation, which is the spelling for an upstream whose members are not interchangeable.
///
/// The parameters belong to the **upstream**, not to a member: how patient a fleet is with a broken
/// machine is a property of the fleet. They travel with the document, so a reload that re-tunes them
/// changes the judgment immediately without resetting what the gateway observed about a member — the
/// counters and the ejection state are keyed by the member's identity alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ejection {
    max_fails: u32,
    fail_timeout_ms: u32,
}

impl Ejection {
    /// Builds ejection parameters from already validated values.
    pub(super) const fn new(max_fails: u32, fail_timeout_ms: u32) -> Self {
        Self {
            max_fails,
            fail_timeout_ms,
        }
    }

    /// Returns the consecutive failures that eject a member; `0` means ejection is disabled.
    #[must_use]
    pub const fn max_fails(&self) -> u32 {
        self.max_fails
    }

    /// Returns how long an ejected member stays out of rotation before one probe is admitted.
    #[must_use]
    pub const fn fail_timeout_ms(&self) -> u32 {
        self.fail_timeout_ms
    }
}

impl Default for Ejection {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_FAILS, DEFAULT_FAIL_TIMEOUT_MS)
    }
}

/// The transport scheme used to reach an [`Upstream`].
///
/// The set is closed for this milestone but may grow, so matches from other crates must be
/// exhaustive over the variants they know plus a wildcard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Scheme {
    /// Cleartext HTTP.
    Http,
    /// HTTP over TLS.
    Https,
}

/// How the data plane chooses a member of one [`Upstream`].
///
/// The spelling in a document is the variant name in `snake_case`. The set is deliberately **closed**
/// — unlike [`Scheme`], this enum has no `#[non_exhaustive]` — because the data plane turns it into a
/// picker in one exhaustive match. A new variant must therefore fail to compile until it has an
/// implementation, instead of silently selecting by a default arm.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// The members in document order, one request each.
    ///
    /// This is the default because it needs no configuration and no key: with one member it is the
    /// only possible answer, and with several it spreads requests evenly without keeping state about
    /// the caller.
    #[default]
    RoundRobin,
    /// The members in proportion to their `weight`.
    ///
    /// Smooth rather than in bursts: the chosen member's running weight is reduced by the total, so a
    /// `3:1` pair interleaves instead of sending three requests and then one.
    WeightedRoundRobin,
    /// The member with the fewest requests in flight, breaking ties in turn.
    ///
    /// The count is the data plane's own — Pingora's pool is process-wide and its connections belong
    /// to no request — and it is held for as long as a request uses the member.
    LeastConn,
    /// The caller's address, hashed onto a member.
    ///
    /// The same caller keeps landing on the same member while the member set is unchanged, which is
    /// what a stateful or cache-warming upstream needs. The key is the caller's address and never a
    /// header, so it cannot be chosen by the caller.
    IpHash,
}

/// One address an [`Upstream`] may be reached at.
///
/// `address` is the connection target and `port` its port; neither participates in the `Host` header
/// or the TLS server name, which come from [`Upstream::host`]. That split is what lets one certificate
/// and one virtual host sit behind several machines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub(super) name: Arc<str>,
    pub(super) address: Arc<str>,
    pub(super) port: u16,
    pub(super) weight: u16,
}

impl Endpoint {
    /// Builds a member from values the parser has already defaulted.
    pub(super) fn new(
        name: impl Into<Arc<str>>,
        address: impl Into<Arc<str>>,
        port: u16,
        weight: u16,
    ) -> Self {
        Self {
            name: name.into(),
            address: address.into(),
            port,
            weight,
        }
    }

    /// Returns the member's name, unique within its upstream.
    ///
    /// It is the identifier logs and metrics use — not `address:port`, which changes when a machine is
    /// replaced while the role it plays does not.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the address a request to this member connects to.
    #[must_use]
    pub fn address(&self) -> &str {
        &self.address
    }

    /// Returns the member's port.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Returns the member's share of the traffic, in `1..=1000`.
    ///
    /// Zero is refused by validation: in a traffic-layer member list it would mean "never choose me",
    /// which is a different statement than a weight and has no fallback meaning here.
    #[must_use]
    pub const fn weight(&self) -> u16 {
        self.weight
    }
}

/// An HTTP destination referenced by a [`Route`](super::Route).
///
/// Fields are private and there is no public constructor: an `Upstream` only exists inside a
/// [`Document`](super::Document) that has passed reference and semantic validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    pub(super) name: Arc<str>,
    pub(super) scheme: Scheme,
    pub(super) host: Arc<str>,
    pub(super) port: u16,
    pub(super) strategy: Strategy,
    pub(super) endpoints: Arc<[Endpoint]>,
    pub(super) ejection: Ejection,
}

impl Upstream {
    /// Returns the unique upstream name referenced by routes.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the transport scheme, which every member of this upstream shares.
    #[must_use]
    pub const fn scheme(&self) -> Scheme {
        self.scheme
    }

    /// Returns the logical host: the value of the `Host` header and of the TLS server name.
    ///
    /// It is never a member's `address`. A certificate names a service, and one service may run on
    /// many machines; writing the machine into `Host` would address a virtual host nobody declared.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Returns the upstream's default port, which a member that writes none inherits.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Returns how the data plane chooses between the members.
    #[must_use]
    pub const fn strategy(&self) -> Strategy {
        self.strategy
    }

    /// Returns the members a request may be sent to, in document order.
    ///
    /// Never empty: a document that names no member is expanded into one, so a selector always has a
    /// candidate and no caller needs a "single instance" branch.
    #[must_use]
    pub fn endpoints(&self) -> &[Endpoint] {
        &self.endpoints
    }

    /// Returns the failure budget every member of this upstream is judged by.
    #[must_use]
    pub const fn ejection(&self) -> Ejection {
        self.ejection
    }
}

#[cfg(test)]
mod tests;
