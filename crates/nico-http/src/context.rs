//! Per-request state: the one snapshot a request sees, the peer it goes to, and each budget.
//!
//! [`HttpContext`] is the data plane's own state, not part of the plugin contract: a plugin reaches a
//! narrowed view of it through [`PluginContext`](crate::PluginContext), and never sees the session,
//! the header structures or the body it is charged for. The split is what lets two loading mechanisms
//! share one interface — a WASM guest is handed a value, never this type.
//!
//! What the context deliberately does not expose is the session and the raw body: a policy cannot
//! write a response behind the data plane's back, and cannot consume body bytes that Pingora will not
//! replay. What a policy *can* do is record where the request goes
//! ([`HttpContext::set_destination`]), which is how a filter that understands a provider target hands
//! it to the data plane.
//!
//! The context copies the few fields it needs — route id, target name, method, path — instead of
//! borrowing them from the document. A borrow would make the context self-referential, because it
//! also owns the `Arc<RuntimeSnapshot>` that keeps those values alive; copying a handful of small
//! strings per request is the cheaper trade.
//!
//! Fields mean different things and each has its own rule (a deadline saturates at zero, a body
//! counter only grows, an error slot keeps the first failure), so they are addressed by named
//! accessors rather than by one field enum: there is no shared table of names to keep in sync.
//!
//! The budget is owned here, so the two facts derived from it are too: [`HttpContext::expired`] and
//! [`HttpContext::deadline_error`]. Every hook that must stop when time is up asks the context
//! instead of re-deriving the comparison from `deadline()` and reporting its own elapsed time.
//!
//! The identity a policy established is a typed slot ([`Identity`]), not a request header: a header
//! can be forged by the client and read by every other plugin, while this slot can only be written by
//! a plugin that authenticated the request and cannot carry anything the caller sent. The same rule
//! covers the peer address ([`HttpContext::client_ip`]): it is recorded from the connection, so a
//! policy that keys on the caller never reads a value the caller chose.

mod attempt;
mod buffer;
mod identity;
mod llm;
mod member;
mod plugins;
mod proxy;
mod walk;

use std::{
    net::IpAddr,
    sync::Arc,
    time::{Duration, Instant},
};

pub(crate) use attempt::FailureStage;
pub use identity::Identity;
use nico_core::{Generation, Route, RouteTarget, RuntimeSnapshot};
use pingora::http::{Method, RequestHeader};

pub(crate) use self::walk::TrafficWalk;
use self::{
    buffer::BodyBuffer,
    llm::LlmExchange,
    proxy::{ProxyHeaders, ResponseSpool},
};
use crate::{
    budget,
    error::HttpError,
    plugin::PluginChain,
    quota::BudgetStore,
    upstream::{
        Destination,
        pools::{ConnectionGuard, SelectionTable},
    },
};

/// What the data plane remembers about one in-flight request.
///
/// Built empty by `new_ctx`, filled by [`HttpContext::begin`] once a route has matched, then read by
/// the hooks that follow. A request that never matched a route keeps an empty context, which is why
/// every accessor is optional.
#[derive(Debug, Default)]
pub struct HttpContext {
    snapshot: Option<Arc<RuntimeSnapshot>>,
    route_id: Option<Arc<str>>,
    /// The route's per-identity token budget, when it declares one.
    token_budget: Option<nico_core::TokenBudget>,
    /// The process-level token-budget table, cloned from the state handle at `begin`.
    budgets: Option<Arc<dyn BudgetStore>>,
    target: Option<RouteTarget>,
    method: Option<Method>,
    path: Option<String>,
    started: Option<Instant>,
    deadline: Option<Instant>,
    destination: Option<Destination>,
    /// The members this request may select from: its own generation's list.
    selection: Option<Arc<SelectionTable>>,
    /// The in-flight count the selected member holds until this context is dropped.
    connection: Option<ConnectionGuard>,
    client_ip: Option<IpAddr>,
    body_limit: Option<u32>,
    response_limit: Option<u32>,
    body_bytes: u64,
    llm: Option<LlmExchange>,
    /// One traffic request's walk over its upstream's members, when the route has a failover
    /// budget. `None` for a converting route (its walk is the LLM attempt) and before the request
    /// selects its first member.
    walk: Option<TrafficWalk>,
    /// How many members one request may walk, from the route's `retry_budget`; `1` until a route is
    /// bound, which is the shape every route had before failover existed.
    failover_budget: u8,
    /// Route request-header rewrites cached at bind time.
    proxy_headers: ProxyHeaders,
    /// Response buffering decision and spool state for this upstream exchange.
    proxy: ResponseSpool,
    /// The route's path rewrite: the matched prefix and its replacement.
    rewrite: Option<(Arc<str>, Arc<str>)>,
    /// The client's original `Host`, captured before automatic upstream rewriting.
    client_host: Option<Arc<str>>,
    error: Option<HttpError>,
    global: Option<PluginChain>,
    route_chain: Option<PluginChain>,
    buffered: bool,
    rewrites_response: bool,
    request_buffer: BodyBuffer,
    identity: Option<Identity>,
    failed_plugin: Option<Arc<str>>,
    cancelled: bool,
    trace_id: String,
}

impl HttpContext {
    /// Records the generation and the document-wide budget, before anything is matched.
    ///
    /// Routing comes second because a global plugin runs *before* it, and a plugin stage runs inside
    /// the request's remaining time: without a deadline here, the first stage of every request would
    /// be the one stage no budget covers. What this sets is the document's own `request` bound; the
    /// route can only tighten it ([`HttpContext::bind_route`]).
    ///
    /// The snapshot is borrowed and cloned into the context rather than taken by value: the caller
    /// owns it for the request anyway, and the context has to hold a handle to outlive that borrow.
    ///
    /// `now` is the instant the request started: the deadline is derived from it so a test can
    /// drive the clock instead of sleeping.
    pub fn begin(&mut self, snapshot: &Arc<RuntimeSnapshot>, header: &RequestHeader, now: Instant) {
        let request = snapshot.document().request();
        self.method = Some(header.method.clone());
        self.path = Some(header.uri.path().to_owned());
        self.started = Some(now);
        self.deadline = Some(now + budget::document_budget(request));
        self.body_limit = Some(request.max_body_bytes());
        self.snapshot = Some(Arc::clone(snapshot));
        // One attempt until a route says otherwise: the failover budget is the route's to set, and
        // the document shape before it binds is the shape every route had before failover existed.
        self.failover_budget = 1;
    }

    /// Records what the request matched, tightening the budget the document set.
    ///
    /// A route may only make the deadline earlier and the body limit smaller, which is the same
    /// "inner may tighten, never widen" rule the budget arithmetic applies — and it is why the deadline
    /// is recomputed from the start instant rather than extended from the current one.
    ///
    /// Called once, after routing, by whichever hook matched the route. A rejected request may never
    /// reach it and keeps the document budget.
    pub fn bind_route(&mut self, route: &Route) {
        self.route_id = Some(Arc::from(route.id()));
        self.target = Some(route.target().clone());
        let Some(request) = self
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.document().request())
        else {
            return;
        };
        self.body_limit = Some(budget::body_limit(route, request));
        self.response_limit = Some(budget::response_limit(route, request));
        // The budget is a route fact cached beside the other two: the check at peer time and the
        // charge at record time read the context instead of reaching back into the snapshot.
        self.token_budget = route.token_budget().cloned();
        // The failover budget is the same kind of route fact: the walk reads the context, not the
        // snapshot, so a reload mid-request cannot change what this request may walk.
        self.failover_budget = route.retry_budget().max(1);
        self.proxy_headers.bind(route);
        self.proxy
            .set_route(route.proxy_buffering(), route.max_buffer_size());
        self.rewrite = route
            .rewrite_prefix()
            .map(|value| (Arc::from(route.path_prefix()), Arc::from(value)));
        if let Some(started) = self.started {
            self.deadline = Some(started + budget::request_budget(route, request));
        }
    }

    /// Returns the snapshot this request is served from, once it has begun.
    ///
    /// The `Arc` is held for the whole request, so the generation cannot change under it.
    #[must_use]
    pub fn snapshot(&self) -> Option<&RuntimeSnapshot> {
        self.snapshot.as_deref()
    }

    /// Returns the generation this request is served from, for logging.
    #[must_use]
    pub fn generation(&self) -> Option<Generation> {
        self.snapshot.as_ref().map(|snapshot| snapshot.generation())
    }

    /// Returns the matched route id, for logging.
    #[must_use]
    pub fn route_id(&self) -> Option<&str> {
        self.route_id.as_deref()
    }

    /// Records the process-level token-budget table this request checks and charges against.
    pub(crate) fn set_budget_store(&mut self, store: Arc<dyn BudgetStore>) {
        self.budgets = Some(store);
    }

    /// Returns the table this request charges, when the process has one.
    pub(crate) fn budget_store(&self) -> Option<&dyn BudgetStore> {
        self.budgets.as_deref()
    }

    /// Returns the route's per-identity token budget, when it declares one.
    ///
    /// The budget is a route fact cached at [`HttpContext::bind_route`], so the check at peer time
    /// and the charge at record time read the context instead of reaching back into the snapshot.
    #[must_use]
    pub fn token_budget(&self) -> Option<&nico_core::TokenBudget> {
        self.token_budget.as_ref()
    }

    /// Returns the matched route id as a handle, for a metric label that must not re-allocate it.
    ///
    /// The id is shared with the request context, so a caller that needs it as a `SharedString`
    /// clones one `Arc` instead of copying the bytes.
    #[must_use]
    pub(crate) fn route_id_handle(&self) -> Option<&Arc<str>> {
        self.route_id.as_ref()
    }

    pub(crate) fn capture_client_host(&mut self, host: &str) {
        if self.client_host.is_none() {
            self.client_host = Some(Arc::from(host));
        }
    }

    /// Returns the client's original `Host`, when it sent one.
    pub(crate) fn original_host(&self) -> Option<&str> {
        self.client_host.as_deref()
    }

    /// Returns the route's target, which names either an upstream or a provider.
    #[must_use]
    pub fn target(&self) -> Option<&RouteTarget> {
        self.target.as_ref()
    }

    /// Returns the route's path rewrite: the matched prefix and the value that replaces it.
    ///
    /// The forward recomputes the new path from the client's original path every attempt, which is
    /// what makes the rewrite idempotent across a failover's retry rounds.
    pub(crate) fn rewrite(&self) -> Option<(&str, &str)> {
        self.rewrite
            .as_ref()
            .map(|(prefix, replacement)| (prefix.as_ref(), replacement.as_ref()))
    }

    /// Returns the name the route targets: an upstream name or a provider name.
    ///
    /// This is the `upstream` log field: for a provider route it names the provider the request was
    /// handed to, which is what an operator needs to correlate a failure.
    #[must_use]
    pub fn upstream(&self) -> Option<&str> {
        self.target.as_ref().map(RouteTarget::name)
    }

    /// Records where the request goes.
    ///
    /// Two callers exist and neither can be surprising to the other: a filter that owns the target
    /// kind a route named (a provider) resolves it before an upstream is selected, and the data
    /// plane records the destination it derived for a document upstream. Precedence is decided where
    /// the destination is chosen, not here: a destination a filter recorded is never replaced by the
    /// fallback the data plane would otherwise derive.
    pub fn set_destination(&mut self, destination: Destination) {
        self.destination = Some(destination);
    }

    /// Returns where the request goes, once anything has decided it.
    ///
    /// The value is read by the hook that connects and by the one that rewrites `Host`, so both
    /// describe the same peer even when a filter chose it.
    #[must_use]
    pub fn destination(&self) -> Option<&Destination> {
        self.destination.as_ref()
    }

    /// Returns the request method, for logging and filters.
    #[must_use]
    pub fn method(&self) -> Option<&Method> {
        self.method.as_ref()
    }

    /// Records the address the request arrived from, once the transport reported one.
    ///
    /// Only the data plane calls this, and only from the connection's own peer address: a filter
    /// that wants "who is calling" must never derive it from a header, which is exactly the value a
    /// client controls. The address is recorded rather than the session, because a
    /// [`PluginContext`](crate::PluginContext) deliberately cannot see the session.
    ///
    /// The first address wins. The data plane records it before any filter runs, so a later write
    /// can only come from a policy — and a policy must not be able to choose the address a rate
    /// limit is keyed by. The same rule [`HttpContext::set_identity`] applies to a subject.
    pub fn set_client_ip(&mut self, address: IpAddr) {
        if self.client_ip.is_none() {
            self.client_ip = Some(address);
        }
    }

    /// Returns the address the request arrived from, when the transport reported one.
    ///
    /// `None` means the transport does not expose a peer address (a Unix socket, a test harness);
    /// a policy that keys on the caller must then fall back to a key that does not distinguish
    /// callers, never to a value a caller can choose.
    #[must_use]
    pub const fn client_ip(&self) -> Option<IpAddr> {
        self.client_ip
    }

    /// Returns the request path, for logging and filters. The query string is not part of it.
    #[must_use]
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    /// Returns the whole-request deadline, once the request has begun.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Returns the time left before the deadline, saturating at zero.
    ///
    /// `None` means the request has not begun, so it has no budget yet — different from "expired",
    /// which is `Some(Duration::ZERO)`.
    #[must_use]
    pub fn remaining(&self, now: Instant) -> Option<Duration> {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    /// Reports whether the request has spent its whole budget.
    ///
    /// A context that never began is not expired: an unrouted request is rejected by routing, not
    /// by a clock.
    #[must_use]
    pub fn expired(&self, now: Instant) -> bool {
        self.remaining(now)
            .is_some_and(|remaining| remaining.is_zero())
    }

    /// Builds the failure for an expired budget.
    ///
    /// It lives here because the context owns the clock the elapsed time is measured against, so
    /// every hook that gives up reports the same number for the same request.
    #[must_use]
    pub fn deadline_error(&self) -> HttpError {
        HttpError::DeadlineExceeded {
            elapsed_ms: self.elapsed_ms(Instant::now()),
        }
    }

    /// Returns the deadline failure when the request's budget is spent, or `None` while it is alive.
    ///
    /// The context owns both the clock and the error built from it, so a hook never re-derives one
    /// from the other. This is the single guard for every stage `timeout_at` cannot wrap: the
    /// synchronous response-body hook calls it before and after the stage, and the header hooks call
    /// it before committing a response.
    #[must_use]
    pub fn deadline_failure(&self, now: Instant) -> Option<HttpError> {
        self.expired(now).then(|| self.deadline_error())
    }

    /// Returns the milliseconds spent on this request so far, for the `elapsed_ms` log field.
    #[must_use]
    pub fn elapsed_ms(&self, now: Instant) -> u64 {
        self.started.map_or(0, |started| {
            now.saturating_duration_since(started).as_millis() as u64
        })
    }

    /// Returns how many body bytes this route accepts.
    #[must_use]
    pub fn body_limit(&self) -> Option<u32> {
        self.body_limit
    }

    /// Returns the ceiling on a buffered response body this route accepts, in bytes.
    ///
    /// The smaller of the route override and the document bound, decided by [`Self::bind_route`] the
    /// same way the two request limits are. It applies only to a response that is buffered whole; an
    /// event stream is translated chunk by chunk and never held to it.
    #[must_use]
    pub fn response_limit(&self) -> Option<u32> {
        self.response_limit
    }

    /// Returns how many body bytes have been counted so far.
    #[must_use]
    pub fn body_bytes(&self) -> u64 {
        self.body_bytes
    }

    /// Adds a received body chunk to the count.
    pub fn count_body(&mut self, bytes: usize) {
        self.body_bytes = self
            .body_bytes
            .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
    }

    /// Returns the limit the counted body exceeded, or `None` while it is within the limit.
    ///
    /// The verdict and the limit it was measured against are one query: a caller never has to assume
    /// a limit exists after being told the body is over it, and no request path needs `expect`.
    #[must_use]
    pub fn exceeded_body_limit(&self) -> Option<u32> {
        self.body_limit
            .filter(|&limit| budget::over_limit(limit, self.body_bytes))
    }

    /// Stores a failure for the hook that can still act on it.
    ///
    /// The first failure wins: a later symptom of an earlier cause must not replace it.
    pub fn fail(&mut self, error: HttpError) {
        if self.error.is_none() {
            self.error = Some(error);
        }
    }

    /// Returns the stored failure without taking it, when a hook only needs to inspect it.
    #[must_use]
    pub fn error(&self) -> Option<&HttpError> {
        self.error.as_ref()
    }

    /// Records the identity an authenticating filter established.
    ///
    /// Only an authentication filter calls this, and only after it verified the credential. The
    /// first identity wins: a later filter cannot replace a subject another filter already claimed,
    /// which would let a policy change who a rate limit is charged to.
    pub fn set_identity(&mut self, identity: Identity) {
        if self.identity.is_none() {
            self.identity = Some(identity);
        }
    }

    /// Returns the identity a filter established, when one did.
    ///
    /// A rate limit reads it to key its bucket, and the request log reads its
    /// [`source`](Identity::source) to attribute the authentication. `None` means the request was not
    /// authenticated, which is a different fact from "authenticated as nobody".
    #[must_use]
    pub fn identity(&self) -> Option<&Identity> {
        self.identity.as_ref()
    }

    /// Records the correlation id for this request's log line and error bodies.
    ///
    /// It is written once, before routing, so every later hook — including the ones that answer a
    /// request that matched no route — reports the same value the caller can quote back.
    pub fn set_trace_id(&mut self, trace_id: &str) {
        self.trace_id.clear();
        self.trace_id.push_str(trace_id);
    }

    /// Returns the correlation id, or `""` before one was recorded.
    #[must_use]
    pub fn trace_id(&self) -> &str {
        &self.trace_id
    }
}

#[cfg(test)]
mod tests;
