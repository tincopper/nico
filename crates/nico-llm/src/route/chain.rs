//! The compiled attempt chain of one converting route: every target the route may serve, in order.
//!
//! A converting route's document names one provider target and, optionally, the providers tried
//! after it. Publication compiles the whole sequence once: each member is a full [`Exchange`] — the
//! shared client codec paired with that member's own adapter, endpoint and credential — so a request
//! never re-resolves anything between attempts, and a member the build cannot serve refuses the
//! publication instead of failing every request that falls back to it.
//!
//! The chain also carries each target's [`TargetIdentity`]: the facts a target's long-lived state is
//! keyed by across generations (the provider name, the base URL, the kind and the breaker
//! parameters). Two generations that compile an equal identity continue one state — the same rule
//! the rate-limit table follows for a quota that has not changed — and a changed identity starts
//! over, because a state that outlives its own parameters describes an endpoint that no longer
//! exists.
//!
//! The capability set of the chain is the **intersection** of its members' sets: a fallback target
//! can only serve what every target on the route serves, so a request that needs more is refused
//! once, before any target is contacted, rather than discovering the gap halfway down the chain.

use std::{fmt, sync::Arc};

use nico_http::{LlmRejection, error::HttpError};

use crate::{
    Exchange, ExchangeError, protocol::Capabilities, provider::ProviderKind, schema::CanonicalError,
};

/// Why an attempt chain could not be built.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("an attempt chain needs at least one target")]
pub struct ChainBuildError;

/// One target of an attempt chain: a compiled exchange and the identity its state is keyed by.
///
/// The exchange is shared through an `Arc` because the per-response transform of one request holds
/// it for the response's whole lifetime; the chain itself is published once and read by every
/// request, so a member clone is a pointer bump, never a re-compile.
pub struct ChainMember {
    exchange: Arc<Exchange>,
    identity: TargetIdentity,
}

impl ChainMember {
    /// Pairs a compiled exchange with the identity its target's state is kept under.
    #[must_use]
    pub fn new(exchange: Arc<Exchange>, identity: TargetIdentity) -> Self {
        Self { exchange, identity }
    }

    /// Returns the member's compiled exchange, for the reads a publication and a request make.
    #[must_use]
    pub fn exchange(&self) -> &Exchange {
        self.exchange.as_ref()
    }

    /// Returns a shared handle to the member's compiled exchange, for a value that outlives the
    /// call that took it — the per-response transform holds its exchange for the response's whole
    /// lifetime.
    #[must_use]
    pub fn shared_exchange(&self) -> Arc<Exchange> {
        Arc::clone(&self.exchange)
    }

    /// Returns the identity this target's long-lived state is keyed by.
    #[must_use]
    pub const fn identity(&self) -> &TargetIdentity {
        &self.identity
    }
}

impl fmt::Debug for ChainMember {
    /// Names the target and its breaker parameters, and delegates the rest to the exchange's own
    /// `Debug`, which reports headers by name and never by value. The base URL is deliberately
    /// absent: it is endpoint configuration, and this value reaches the publication log.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChainMember")
            .field("identity", &self.identity)
            .field("exchange", &self.exchange)
            .finish()
    }
}

/// The breaker parameters one target's state machine runs with.
///
/// The values are the document's, materialized to the built-in defaults where it wrote none; the
/// type is copied value, because the parameters are part of the target's identity rather than a
/// shared handle a generation could re-point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BreakerParams {
    threshold: u32,
    cooldown_ms: u32,
}

impl BreakerParams {
    /// Builds breaker parameters from validated document values.
    #[must_use]
    pub const fn new(threshold: u32, cooldown_ms: u32) -> Self {
        Self {
            threshold,
            cooldown_ms,
        }
    }

    /// Returns the consecutive failures that open the breaker; `0` means it is disabled.
    #[must_use]
    pub const fn threshold(&self) -> u32 {
        self.threshold
    }

    /// Returns how long an open breaker stays open before it admits one probe.
    #[must_use]
    pub const fn cooldown_ms(&self) -> u32 {
        self.cooldown_ms
    }

    /// Returns whether this target's breaker is disabled.
    #[must_use]
    pub const fn is_disabled(&self) -> bool {
        self.threshold == 0
    }
}

/// The identity a compiled target keeps its long-lived state under, across generations.
///
/// An equal identity means "the same endpoint, configured the same way": the provider name the
/// document gives it, the base URL it addresses, the adapter kind that serves it and the breaker
/// parameters it runs with. The state a generation leaves behind (a breaker that opened, a failure
/// count) is inherited by the next generation that compiles an equal identity and dropped by one
/// that does not, so a re-tuned endpoint starts its breaker over instead of opening under thresholds
/// it no longer has.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TargetIdentity {
    provider: Arc<str>,
    base_url: Arc<str>,
    kind: ProviderKind,
    params: BreakerParams,
}

impl TargetIdentity {
    /// Names one compiled target.
    #[must_use]
    pub fn new(provider: &str, base_url: &str, kind: ProviderKind, params: BreakerParams) -> Self {
        Self {
            provider: Arc::from(provider),
            base_url: Arc::from(base_url),
            kind,
            params,
        }
    }

    /// Returns the provider name, as the document wrote it.
    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// Returns the base URL the target addresses.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Returns the adapter kind that serves the target.
    #[must_use]
    pub const fn kind(&self) -> ProviderKind {
        self.kind
    }

    /// Returns the breaker parameters the target runs with.
    #[must_use]
    pub const fn params(&self) -> BreakerParams {
        self.params
    }
}

impl fmt::Debug for TargetIdentity {
    /// Names the provider and the kind, and the breaker parameters, but never the base URL: this
    /// value is printed by the publication log and the circuit log, and endpoint configuration does
    /// not belong in either.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TargetIdentity")
            .field("provider", &self.provider)
            .field("kind", &self.kind)
            .field("params", &self.params)
            .finish()
    }
}

/// One converting route's compiled attempt sequence: its primary target followed by the fallback
/// members, in document order.
///
/// Built once per route at publication and held by the data plane through the converting front-end.
/// Every member is a full exchange, so serving attempt *n* is a lookup plus an encode — nothing is
/// resolved, decoded or re-paired while a request is being retried.
pub struct AttemptChain {
    members: Vec<ChainMember>,
    capabilities: Capabilities,
}

impl AttemptChain {
    /// Builds a chain from the compiled members, in attempt order.
    ///
    /// # Errors
    /// Returns [`ChainBuildError`] when `members` is empty. A published route always has a primary
    /// target, so a caller reaching this has skipped its own compile step; the refusal keeps the
    /// publication panic-free instead of trusting an invariant the type cannot see.
    pub fn try_new(members: Vec<ChainMember>) -> Result<Self, ChainBuildError> {
        let Some(first) = members.first() else {
            return Err(ChainBuildError);
        };
        // The chain serves what every member can serve: one target that lacks a capability would
        // fail a request the earlier members already accepted, after the caller had reason to expect
        // an answer. Folding here keeps the refusal at publication time, where the route's whole
        // capability set is one decision.
        let capabilities = members[1..]
            .iter()
            .fold(first.exchange().capabilities(), |rest, member| {
                rest.intersection(member.exchange().capabilities())
            });
        Ok(Self {
            members,
            capabilities,
        })
    }

    /// Returns the members in attempt order: the primary target first, the fallback members after.
    #[must_use]
    pub fn members(&self) -> &[ChainMember] {
        &self.members
    }

    /// Returns the primary target, which serves every attempt until the chain moves on.
    #[must_use]
    pub fn primary(&self) -> &ChainMember {
        // INVARIANT: `try_new` refuses an empty member list, so the first member always exists.
        &self.members[0]
    }

    /// Returns the capabilities every member shares, which the request path enforces.
    #[must_use]
    pub const fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    /// Turns a failed exchange into the client protocol's error answer.
    ///
    /// The status, code, category and body are the exchange's: the canonical error owns the first
    /// three, and the client codec owns the document. Only a codec that cannot encode its own error
    /// envelope reaches the fallback, which is the gateway's defect rather than the caller's.
    pub(crate) fn reject(
        &self,
        canonical: CanonicalError,
        exchange_error: Option<&ExchangeError>,
    ) -> HttpError {
        let exchange = self.primary().exchange();
        let reply = match exchange_error {
            Some(error) => exchange.error_reply(error),
            // The canonical value stands on its own: the codec renders it directly.
            None => exchange.encode_error_reply(&canonical),
        };
        let reply = match reply {
            Ok(reply) => reply,
            Err(source) => return HttpError::internal(source),
        };
        match LlmRejection::new(
            reply.status(),
            canonical.code(),
            canonical.message(),
            reply.into_body(),
            canonical.category(),
        ) {
            Ok(rejection) => HttpError::LlmRejected {
                rejection: Arc::new(rejection),
            },
            // The canonical error validated its own code, message and status before the codec could
            // see it, so a refusal here means the mapping above produced something inconsistent.
            Err(source) => HttpError::internal(source),
        }
    }
}

impl fmt::Debug for AttemptChain {
    /// Reports how many targets the chain holds and the capability set it serves, and delegates the
    /// rest: the members' own `Debug` names no credential, header value or prompt.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AttemptChain")
            .field("targets", &self.members.len())
            .field("capabilities", &self.capabilities)
            .field("primary", &self.primary())
            .finish()
    }
}

#[cfg(test)]
mod tests;
