//! Routes: the unit that decides where a request goes.
//!
//! A [`Route`] carries only what routing needs before the request body is read: an optional exact
//! host, a path prefix, and exactly one target. The target is a [`RouteTarget`] rather than two
//! optional fields, so "no target" and "two targets" are unrepresentable once the document is
//! built. Prefix semantics — how `/api/` matches `/api/x` — belong to the data plane, not here.
//!
//! The route also names the plugins that run on its requests. It names them rather than carrying
//! them, so one plugin entry is shared by every route that references it and a route cannot
//! disagree with the plugin it runs.
//!
//! A provider target additionally decides how the exchange is translated: [`Route::protocol`] names
//! the client protocol, [`Route::mode`] whether the body is converted or passed through, and
//! [`Route::max_response_bytes`] how much of a buffered response is accepted. Those three describe a
//! converted exchange, so they exist on a provider route alone; an upstream route forwards bytes and
//! the document refuses them there instead of ignoring them. The same holds for the attempt fields —
//! [`Route::retry_budget`] bounds the tries one target absorbs and [`Route::fallback`] names the
//! providers tried after it — because both decide how a *converted* exchange may fail, which an
//! upstream route has no way to observe.

use serde::Deserialize;

/// Where a matched route sends the request.
///
/// A data plane must handle the target kinds it knows plus a wildcard: the set is closed for this
/// milestone but a new kind arrives with a new data plane, not with a change to an existing one.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RouteTarget {
    /// Forward to the named [`Upstream`](super::Upstream).
    Upstream(String),
    /// Hand the request to the named [`Provider`](super::Provider).
    Provider(String),
}

impl RouteTarget {
    /// Returns the referenced name, whichever kind of target this is.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Upstream(name) | Self::Provider(name) => name,
        }
    }
}

/// How a provider route relates the client's body to the provider's.
///
/// The mode is a property of the route, not of the protocol pair: the same two protocols may be
/// byte-compatible for one route and not for another (a caller sending an extension the canonical
/// model does not carry, for example), so the document decides per route. `Convert` is the default
/// because it is the mode that works between two different protocols at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum RouteMode {
    /// Decode the client body into the canonical model, then encode it for the provider.
    #[default]
    Convert,
    /// Forward the client's bytes to the provider unchanged, skipping the canonical model.
    Passthrough,
}

/// How a route pool chooses the member one attempt (or request) is sent to.
///
/// The set is closed and deliberately separate from [`Strategy`](super::Strategy): the traffic layer
/// chooses between addresses (a connection, an in-flight count, a caller's address), while a provider
/// pool chooses between services. The strategies a pool of either kind may name are the ones that
/// mean the same thing on both sides — a proportional draw, a document-order rotation, and a
/// caller-address rendezvous — which is why there is one enum and not two.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PoolStrategy {
    /// Draw a member in proportion to its weight.
    ///
    /// The default: it is the only distribution that does not drift when the member set changes, and
    /// it needs no state about the caller.
    #[default]
    WeightedRandom,
    /// The members in document order, one attempt each.
    RoundRobin,
    /// Rendezvous-hash the caller's full address onto a member, so the same caller keeps meeting the
    /// same member across requests — the sticky form of a canary.
    ///
    /// A caller with no address (a session a test built, a Unix socket peer) is spread by rotation
    /// instead: inventing a key would pile every such caller onto one member. The fallback is the
    /// rule's own, not the pool's.
    IpHash,
}

/// One member of a provider pool: a declared provider, and its share of the traffic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolMember {
    pub(super) name: String,
    pub(super) weight: u16,
}

impl PoolMember {
    /// Builds a member from values the parser has already defaulted.
    pub(super) fn new(name: String, weight: u16) -> Self {
        Self { name, weight }
    }

    /// Returns the name of the [`Provider`](super::Provider) this member serves from.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns this member's share of the traffic, in `0..=1000`.
    ///
    /// Zero means **not a distribution candidate**: the member is only used when a fallback chain
    /// names it explicitly. That is the opposite of the traffic layer, where zero is refused, and the
    /// difference comes from the domain: a provider pool has a fallback concept and an address list
    /// with weights does not.
    #[must_use]
    pub const fn weight(&self) -> u16 {
        self.weight
    }
}

/// The providers one route may send an attempt to, and how it chooses among them.
///
/// A pool replaces the route's single `provider` target. The members are declared providers, resolved
/// while the document is validated, so "a member nobody declared" is a publication error rather than
/// a request-time refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pool {
    pub(super) members: Vec<PoolMember>,
    pub(super) strategy: PoolStrategy,
}

impl Pool {
    /// Builds a pool from values the parser has already validated.
    pub(super) fn new(members: Vec<PoolMember>, strategy: PoolStrategy) -> Self {
        Self { members, strategy }
    }

    /// Returns the members, in document order.
    #[must_use]
    pub fn members(&self) -> &[PoolMember] {
        &self.members
    }

    /// Returns how the pool chooses a member for one attempt.
    #[must_use]
    pub const fn strategy(&self) -> PoolStrategy {
        self.strategy
    }
}

/// One member of an upstream pool: a declared upstream, and its share of the traffic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamPoolMember {
    pub(super) upstream: String,
    pub(super) weight: u16,
}

impl UpstreamPoolMember {
    /// Builds a member from values the parser has already validated.
    pub(super) fn new(upstream: String, weight: u16) -> Self {
        Self { upstream, weight }
    }

    /// Returns the name of the [`Upstream`](super::Upstream) this member forwards to.
    #[must_use]
    pub fn upstream(&self) -> &str {
        &self.upstream
    }

    /// Returns this member's share of the traffic, in `1..=1000`.
    ///
    /// Zero is refused by validation, and the reason is the domain: a provider pool has a fallback
    /// chain a zero-weight member can still ride, while an upstream route has one attempt — a zero
    /// here would be a member that can never serve, not a fallback.
    #[must_use]
    pub const fn weight(&self) -> u16 {
        self.weight
    }
}

/// The upstreams one traffic route may send a request to, and how it chooses among them.
///
/// A pool replaces the route's single `upstream` target. The members are declared upstreams, resolved
/// while the document is validated, so "a member nobody declared" is a publication error rather than
/// a request-time refusal. The route's target stays the pool's first member, so every rule that
/// already reads a route's target keeps its meaning for a pooled route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamPool {
    pub(super) members: Vec<UpstreamPoolMember>,
    pub(super) strategy: PoolStrategy,
}

impl UpstreamPool {
    /// Builds a pool from values the parser has already validated.
    pub(super) fn new(members: Vec<UpstreamPoolMember>, strategy: PoolStrategy) -> Self {
        Self { members, strategy }
    }

    /// Returns the members, in document order.
    #[must_use]
    pub fn members(&self) -> &[UpstreamPoolMember] {
        &self.members
    }

    /// Returns how the pool chooses the upstream one request is sent to.
    #[must_use]
    pub const fn strategy(&self) -> PoolStrategy {
        self.strategy
    }
}

/// The token budget one route's converting requests are bounded by, per authenticated identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenBudget {
    pub(super) limit_tokens: u64,
    pub(super) window_seconds: u32,
}

impl TokenBudget {
    /// Builds a budget from values the parser has already validated.
    pub(super) const fn new(limit_tokens: u64, window_seconds: u32) -> Self {
        Self {
            limit_tokens,
            window_seconds,
        }
    }

    /// Returns the most tokens one identity may spend in one window.
    #[must_use]
    pub const fn limit_tokens(&self) -> u64 {
        self.limit_tokens
    }

    /// Returns how long one budget window lasts, in seconds.
    #[must_use]
    pub const fn window_seconds(&self) -> u32 {
        self.window_seconds
    }
}

/// One routing rule.
///
/// Fields are private and there is no public constructor: a `Route` only exists inside a
/// [`Document`](super::Document) that has passed reference and semantic validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub(super) id: String,
    pub(super) host: Option<String>,
    pub(super) path_prefix: String,
    pub(super) target: RouteTarget,
    pub(super) timeout_ms: Option<u32>,
    /// How long a streaming attempt may wait for its first response byte.
    pub(super) ttft_timeout_ms: Option<u32>,
    pub(super) max_body_bytes: Option<u32>,
    pub(super) protocol: Option<String>,
    pub(super) mode: RouteMode,
    pub(super) max_response_bytes: Option<u32>,
    pub(super) plugins: Vec<String>,
    pub(super) retry_budget: u8,
    pub(super) fallback: Vec<String>,
    /// The token budget, when the route bounds what each identity may spend.
    pub(super) token_budget: Option<TokenBudget>,
    /// The provider pool, when the document named several providers instead of one.
    ///
    /// `None` for a single-provider route and for every upstream route; the two forms are mutually
    /// exclusive by construction, so a route never has both a `Provider` target and a pool.
    pub(super) pool: Option<Pool>,
    /// The upstream pool, when the document named several upstreams instead of one.
    ///
    /// `None` for a single-upstream route and for every provider route; the forms are mutually
    /// exclusive by construction. A pooled route's target is its pool's first member, so rules that
    /// read a target keep their meaning.
    pub(super) upstream_pool: Option<UpstreamPool>,
}

impl Route {
    /// Returns the unique route identifier, used in logs and audit records.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the exact host this route is limited to, when it is not host-independent.
    #[must_use]
    pub fn host(&self) -> Option<&str> {
        self.host.as_deref()
    }

    /// Returns the path prefix this route matches.
    #[must_use]
    pub fn path_prefix(&self) -> &str {
        &self.path_prefix
    }

    /// Returns the upstream or provider this route forwards to.
    #[must_use]
    pub const fn target(&self) -> &RouteTarget {
        &self.target
    }

    /// Returns the upstream pool, when the route distributes across several upstreams.
    ///
    /// `None` for a route that names one upstream, and for every provider route — a provider route's
    /// pool is [`Route::pool`], and the two domains keep their own vocabulary.
    #[must_use]
    pub const fn upstream_pool(&self) -> Option<&UpstreamPool> {
        self.upstream_pool.as_ref()
    }

    /// Returns the route-level timeout in milliseconds, when the document sets one.
    #[must_use]
    pub const fn timeout_ms(&self) -> Option<u32> {
        self.timeout_ms
    }

    /// Returns how long a streaming attempt may wait for its first response byte.
    ///
    /// A budget of its own rather than a share of [`Route::timeout_ms`], because the two bound
    /// different things: the route timeout bounds the whole exchange, while this one bounds the wait
    /// before the first byte — which is the moment a slow provider is worth replacing rather than
    /// waiting out.
    #[must_use]
    pub const fn ttft_timeout_ms(&self) -> Option<u32> {
        self.ttft_timeout_ms
    }

    /// Returns the route-level request body bound in bytes, when the document sets one.
    #[must_use]
    pub const fn max_body_bytes(&self) -> Option<u32> {
        self.max_body_bytes
    }

    /// Returns the client protocol a provider route converts, or `None` for an upstream route.
    ///
    /// The value is a name such as `<vendor>.<surface>`, kept exactly as written. Whether a codec is
    /// registered for it is decided when the document is published, by the crate that owns the
    /// registry: this model only checks that the value could name one.
    #[must_use]
    pub fn protocol(&self) -> Option<&str> {
        self.protocol.as_deref()
    }

    /// Returns how this route relates the client's body to the provider's.
    ///
    /// A provider route defaults to [`RouteMode::Convert`]. An upstream route has no conversion and
    /// the document refuses a `mode` there, so this reports the default for it.
    #[must_use]
    pub const fn mode(&self) -> RouteMode {
        self.mode
    }

    /// Returns the route-level ceiling on a buffered response body, when the document sets one.
    ///
    /// A route may only tighten the document-wide bound, and `None` means it inherits it. Only a
    /// provider route has a response to convert, so the document refuses this on an upstream route.
    #[must_use]
    pub const fn max_response_bytes(&self) -> Option<u32> {
        self.max_response_bytes
    }

    /// Returns the plugins this route runs, in document order.
    ///
    /// The order is the document's, not the execution order: which phase a plugin belongs to is
    /// decided by the crate that implements it, so a filter written before an authentication plugin
    /// does not run before it. An empty list means the route runs no plugins.
    #[must_use]
    pub fn plugins(&self) -> &[String] {
        &self.plugins
    }

    /// Returns the total attempts this route may make against one target, including the first.
    ///
    /// The document's default is `1`, which is the behavior of a route that never names the field:
    /// one attempt, no retry. The value bounds what one failing target absorbs; on a provider route
    /// moving to another target is the fallback chain's decision, not this budget's, and on an
    /// upstream route it is the member walk's.
    #[must_use]
    pub const fn retry_budget(&self) -> u8 {
        self.retry_budget
    }

    /// Returns the provider names this route tries in order when its target fails, in document
    /// order.
    ///
    /// The names reference [`Provider`](super::Provider) entries, resolved while the document is
    /// validated; whether a member can actually be served is the publication's decision. An empty
    /// list means the route has no fallback, and a `passthrough` route is refused one at
    /// publication because it has no failure classification to retry on.
    #[must_use]
    pub fn fallback(&self) -> &[String] {
        &self.fallback
    }

    /// Returns the provider pool this route distributes across, when it declares one.
    ///
    /// `None` means the route has a single target: the [`Route::target`] it names is the whole
    /// answer. A pooled route's [`target`](Route::target) is the pool's first member, so a caller
    /// that does not know about pools still uses the head of the distribution — the behaviour a
    /// single-target route had before pools existed.
    #[must_use]
    pub const fn pool(&self) -> Option<&Pool> {
        self.pool.as_ref()
    }

    /// Returns the token budget this route enforces per identity, when it declares one.
    ///
    /// Only a converting route has tokens to bound, so the document refuses the field on an upstream
    /// route. Requests without an authenticated identity are not bounded — a deployment that wants
    /// full coverage puts an auth plugin on the route.
    #[must_use]
    pub const fn token_budget(&self) -> Option<&TokenBudget> {
        self.token_budget.as_ref()
    }
}
