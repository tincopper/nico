//! The serde shape of a resources document, before any validation.
//!
//! These types are the only place the YAML spelling lives. They never leave the `config` module:
//! [`Document`](super::Document) stays a validated domain type with private fields, so a caller
//! cannot build one by deserializing YAML. Every struct denies unknown fields and none of them uses
//! `serde(default)` at document level — a missing section is an error, not an empty one, because a
//! truncated document must not look like a working configuration.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::{
    route::RouteMode,
    upstream::{Scheme, Strategy},
};

/// The document exactly as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawDocument {
    #[serde(rename = "apiVersion")]
    pub(super) api_version: String,
    pub(super) request: RawRequest,
    pub(super) providers: Vec<RawProvider>,
    pub(super) upstreams: Vec<RawUpstream>,
    pub(super) routes: Vec<RawRoute>,
    pub(super) plugins: Vec<RawPlugin>,
    /// Names of declared plugins that run for every request, before routing.
    ///
    /// Absent and `[]` mean the same thing — no global policy — so a document that only uses
    /// route-scoped plugins does not have to spell the empty list out.
    #[serde(default)]
    pub(super) global_plugins: Vec<String>,
}

/// The `request` section exactly as written, before range validation.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawRequest {
    pub(super) max_body_bytes: u32,
    /// Absent means the 8 MiB default; the field exists so a document can lower it, not so every
    /// document has to restate it. An `Option` already means "absent", so no `serde` default is
    /// needed — and a strict document type is not where a default belongs.
    pub(super) max_response_bytes: Option<u32>,
    pub(super) timeout_ms: u32,
}

/// One `providers` entry exactly as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawProvider {
    pub(super) name: String,
    pub(super) kind: String,
    pub(super) base_url: String,
    pub(super) credential_ref: String,
    /// `None` and `[]` mean the same thing — no model restriction — so the empty case does not have
    /// to be written out; the field is still rejected when it contains the wrong type. Its entries are
    /// the provider's own model names, enforced on the resolved name while a request is prepared.
    pub(super) model_allowlist: Option<Vec<String>>,
    /// Logical model name → the name this provider expects. `None` and `{}` mean the same thing —
    /// send the caller's name unchanged — so an unmapped provider does not have to write an empty
    /// mapping out.
    pub(super) model_map: Option<BTreeMap<String, String>>,
    /// Output ceiling this endpoint uses when the request names none. `None` means the endpoint
    /// imposes none; `0` is refused by semantic validation, because a ceiling of zero is not a
    /// ceiling. A request that sets its own limit always wins.
    pub(super) max_output_tokens: Option<u32>,
    /// The endpoint's failure budget. Absent and partially written entries take the built-in
    /// defaults; a `failure_threshold` of `0` is the one spelling for "never break this provider",
    /// so the section cannot mean "absent" and mean that too.
    pub(super) circuit_breaker: Option<RawCircuitBreaker>,
}

/// One provider's `circuit_breaker` section exactly as written.
///
/// Both fields are optional on purpose: an entry that only tightens one knob should not have to
/// restate the other, and the defaults they fall back to live with the typed model, not here.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawCircuitBreaker {
    /// Consecutive failures that open the breaker. `0` disables it for this provider.
    pub(super) failure_threshold: Option<u32>,
    /// How long an open breaker stays open before one probe is allowed through.
    pub(super) cooldown_ms: Option<u32>,
}

/// One `upstreams` entry exactly as written.
///
/// `scheme`, `host` and `port` are always required: they are the upstream's logical identity (`Host`
/// and the TLS server name) and the defaults a member inherits. `endpoints` is the optional member
/// list; absent means exactly one member, expanded while the document is validated.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawUpstream {
    pub(super) name: String,
    pub(super) scheme: Scheme,
    pub(super) host: String,
    pub(super) port: u16,
    /// Absent and `[..]` are different documents: an absent list is one member, an empty list is
    /// refused. `None` and `Some([])` therefore cannot be normalized here without losing that.
    pub(super) endpoints: Option<Vec<RawEndpoint>>,
    /// Absent means [`Strategy::RoundRobin`](super::Strategy::RoundRobin).
    pub(super) strategy: Option<Strategy>,
    /// Consecutive failures that eject one member. Absent means the built-in default; `0` is legal
    /// and means "never eject", which is the spelling for an upstream whose members are not
    /// interchangeable.
    pub(super) max_fails: Option<u32>,
    /// How long an ejected member stays out of rotation before one probe is admitted. Absent means
    /// the built-in default; `0` is refused, because a cooldown of zero admits every request as a
    /// probe and therefore ejects nothing at all.
    pub(super) fail_timeout_ms: Option<u32>,
}

/// One `upstreams[].endpoints[]` entry exactly as written.
///
/// A member carries no `scheme`: one upstream has one transport, and mixing two protocols behind one
/// name is two services, which a document writes as two upstreams.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawEndpoint {
    pub(super) name: String,
    /// Absent means the upstream's `host`: the logical name is also where the request goes until the
    /// document splits the two.
    pub(super) address: Option<String>,
    /// Absent means the upstream's `port`.
    pub(super) port: Option<u16>,
    /// Absent means 1; `0` is refused by semantic validation.
    pub(super) weight: Option<u16>,
}

/// One `routes` entry exactly as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawRoute {
    pub(super) id: String,
    pub(super) host: Option<String>,
    pub(super) path_prefix: String,
    /// Exactly one of `upstream` and `provider` must be present; the pair is turned into a
    /// [`RouteTarget`](super::RouteTarget) during validation so both mistakes get their own error.
    pub(super) upstream: Option<String>,
    pub(super) provider: Option<String>,
    /// The token budget one authenticated identity may spend on this route.
    pub(super) token_budget: Option<RawTokenBudget>,
    /// The provider pool, when the route distributes across several providers.
    ///
    /// Mutually exclusive with `provider`; absent means the single-target forms above. `Some([])` is
    /// refused — a pool with no member is a route with no target, which the document already has its
    /// own error for.
    pub(super) providers: Option<Vec<RawPoolMember>>,
    /// The upstream pool, when the route distributes across several upstreams.
    ///
    /// Mutually exclusive with `upstream`; absent means the single-target forms above. `Some([])` is
    /// refused for the same reason a provider pool with no member is. Refused on a `provider` route:
    /// a pool of upstreams is traffic-layer vocabulary, and a converting route's pools are
    /// provider pools.
    pub(super) upstreams: Option<Vec<RawUpstreamPoolMember>>,
    /// The value that replaces the route's matched `path_prefix` when the request is forwarded.
    ///
    /// Absent means the path is forwarded as written. Empty means the prefix is stripped. Only an
    /// upstream route may carry it — a converting route's path belongs to its adapter.
    pub(super) rewrite_prefix: Option<String>,
    /// How the pool chooses a member for one attempt.
    ///
    /// Absent means [`PoolStrategy::WeightedRandom`](super::PoolStrategy::WeightedRandom). Refused on
    /// a route with no pool: a strategy for a set of one is a value nothing reads, and accepting it
    /// would make a typo look like configuration.
    pub(super) strategy: Option<super::PoolStrategy>,
    pub(super) timeout_ms: Option<u32>,
    /// How long a **streaming** attempt may wait for its first response byte before the route moves to
    /// another target.
    ///
    /// Absent means no separate budget: the attempt is bounded by the route's whole-request timeout
    /// like any other. Only a converting route has a stream to cut, so a written value on an upstream
    /// route is refused rather than ignored.
    pub(super) ttft_timeout_ms: Option<u32>,
    pub(super) max_body_bytes: Option<u32>,
    /// The client protocol a provider route converts, kept as written: whether a codec exists for it
    /// is a publication decision of the crate that owns the registry, not a document rule.
    ///
    /// Required on a provider route and refused on an upstream one, so the pair is checked where the
    /// target is resolved rather than here.
    pub(super) protocol: Option<String>,
    /// Absent means [`RouteMode::Convert`].
    pub(super) mode: Option<RouteMode>,
    /// Only a provider route has a response to convert, so this is refused on an upstream one.
    pub(super) max_response_bytes: Option<u32>,
    /// Plugin names this route runs, in document order. Absent and `[]` mean the same thing — no
    /// plugins — so a route without a policy does not have to spell the empty list out.
    pub(super) plugins: Option<Vec<String>>,
    /// Total attempts the route may make against one provider target, including the first. Absent
    /// means `1`: one attempt, no retry — the behavior of a route that never names the field.
    pub(super) retry_budget: Option<u8>,
    /// Provider names the route tries in order when its target fails, in document order. Absent and
    /// `[]` mean the same thing — no fallback — so a single-target route does not have to spell an
    /// empty chain out. Whether a named member can be served is the publication's decision.
    pub(super) fallback: Option<Vec<String>>,
}

/// One `routes[].token_budget` section exactly as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawTokenBudget {
    pub(super) limit_tokens: u64,
    pub(super) window_seconds: u32,
}

/// One `routes[].providers[]` entry exactly as written.
///
/// A member names a declared provider and its share of the traffic. It carries no `base_url` or
/// credential: those belong to the provider entry, which is also what a member without a pool uses,
/// so one endpoint is described once.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawPoolMember {
    pub(super) name: String,
    /// Absent means `1`. `0` is legal and means "not a distribution candidate", which is why this
    /// range starts at zero where the traffic layer's member weights start at one.
    pub(super) weight: Option<u16>,
}

/// One `routes[].upstreams[]` entry exactly as written.
///
/// A member names a declared upstream and its share of the traffic. It carries no endpoints or
/// strategy of its own: those belong to the upstream entry, which is also what a route without a
/// pool uses, so one fleet is described once.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawUpstreamPoolMember {
    pub(super) upstream: String,
    /// Absent means `1`. `0` is refused by validation — a traffic route has one attempt and no
    /// fallback chain, so a member that can never be chosen would be a name without a behaviour.
    pub(super) weight: Option<u16>,
}

/// One `plugins` entry exactly as written.
///
/// The entry is an **envelope**: who it is (`name`), which implementation runs it (`kind`), whether it
/// is live, whether it buffers the request body, how it refuses a caller, and a `config` value this
/// model does not interpret. Every field but `config` is common to all plugins, so the envelope is
/// owned here; what `config` may contain belongs to the crate that implements the kind, which reads it
/// with `deny_unknown_fields` while the document is compiled. That is why the entry is flat and typed
/// instead of a flattened enum per kind: adding a kind changes no document model.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawPlugin {
    pub(super) name: String,
    pub(super) kind: String,
    #[serde(default = "enabled_by_default")]
    pub(super) enabled: bool,
    #[serde(default)]
    pub(super) buffers_body: bool,
    pub(super) error_response: Option<RawErrorResponse>,
    /// The kind's own configuration, kept exactly as written.
    ///
    /// `Value` rather than a typed enum: the document model checks the envelope and nothing else, so
    /// a new kind is a new crate rather than a new variant here. An entry that writes no `config`
    /// carries `Value::Null`, which the owning crate reads as "no fields set".
    #[serde(default)]
    pub(super) config: serde_norway::Value,
}

/// The default of `plugins[].enabled`: a plugin is live unless the document stops it.
///
/// Stopping one is why the field exists: an operator can take an entry out of service without
/// editing every route that names it.
const fn enabled_by_default() -> bool {
    true
}

/// One plugin's rejection presentation exactly as written.
///
/// It stays part of the envelope because refusing a caller is common to every plugin that can refuse:
/// the status, the content type and the body shape do not depend on the kind.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawErrorResponse {
    pub(super) status: Option<u16>,
    pub(super) content_type: Option<String>,
    /// Any YAML value: whether it must be structured or a literal string is decided by the effective
    /// content type when the entry is built, so the raw shape does not have to guess it.
    pub(super) body: Option<serde_norway::Value>,
}
