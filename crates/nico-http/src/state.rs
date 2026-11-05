//! The one published value the request path reads: the snapshot and the policies derived from it.
//!
//! A snapshot and the chains compiled from it come from the same document, so a request that read
//! them separately could see generation *N*'s routes with generation *N-1*'s policies — and for an
//! authentication plugin that combination is fail-open: the older chains may not contain the filter
//! that would have rejected the request.
//!
//! [`HttpState`] removes the possibility instead of documenting it: publication writes one `ArcSwap`
//! slot holding everything a generation serves with, so `current()` returns parts that were built
//! together. One generation carries four sets of facts about its policies, and all four travel in
//! [`PublishedChains`]:
//!
//! - `global` — the chain every request runs before it is routed;
//! - `routes` — the chain each route names, looked up after routing;
//! - `buffered` — the routes whose request body plugins need whole, decided at publication because the
//!   hook that receives a chunk may not re-read configuration;
//! - `rewriting_responses` — the routes whose response body a plugin rewrites, which is what decides
//!   that the upstream's `content-length` cannot be kept and the response is framed as chunked.
//!
//! [`SnapshotHandle`] is still the configuration-state fact source (readiness, degradation, generation
//! for operators); it is not part of the request path, and the reload pipeline writes both in one step.
//!
//! [`SnapshotHandle`]: nico_core::SnapshotHandle

use std::{
    collections::{BTreeSet, HashMap},
    fmt, io,
    sync::Arc,
};

use arc_swap::ArcSwapOption;
use nico_core::{Route, RuntimeSnapshot};

use crate::{error::HttpError, plugin::PluginChain};

/// The configuration one generation serves requests from.
///
/// Built by the reload pipeline, never by a request: the snapshot and the policies inside it describe
/// the same document, and nothing here is mutated after construction.
pub struct HttpState {
    snapshot: Arc<RuntimeSnapshot>,
    policies: PublishedChains,
}

impl HttpState {
    /// Returns the snapshot this generation serves from.
    ///
    /// The `Arc` is the same handle [`HttpContext::begin`](crate::HttpContext::begin) keeps for the
    /// request, which is why it is borrowed rather than cloned: a caller that needs to store it clones
    /// the one pointer.
    #[must_use]
    pub fn snapshot(&self) -> &Arc<RuntimeSnapshot> {
        &self.snapshot
    }

    /// Returns the per-route chains derived from that snapshot.
    #[must_use]
    pub fn chains(&self) -> &RouteChains {
        self.policies.routes()
    }

    /// Returns the chain every request runs before a route is matched.
    ///
    /// The global chain is the document's own `global_plugins` use site: an entry listed there runs
    /// for every request, including a request no route matches, so it sees a request that has no
    /// route id and no identity yet.
    #[must_use]
    pub fn global(&self) -> &PluginChain {
        self.policies.global()
    }

    /// Returns the routes whose request body is delivered to plugins whole.
    ///
    /// Empty for a generation whose entries never ask for the body, which is the zero-cost path for
    /// the body hooks: one lookup, no buffering.
    #[must_use]
    pub fn buffered(&self) -> &BufferedRoutes {
        self.policies.buffered()
    }

    /// Returns whether the response of `route_id` is rewritten by a plugin.
    ///
    /// True means the upstream's `content-length` cannot be kept: the bytes downstream are no longer
    /// the bytes that length described, so the response is framed as chunked instead.
    #[must_use]
    pub fn rewrites_response(&self, route_id: &str) -> bool {
        self.policies.rewrites_response(route_id)
    }
}

impl fmt::Debug for HttpState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpState")
            .field("generation", &self.snapshot.generation())
            .field("global", &self.global())
            .field("chains", &self.chains())
            .field("buffered", &self.buffered())
            .field("rewriting_responses", &self.policies.rewriting_responses())
            .finish()
    }
}

/// The publication handle for [`HttpState`].
///
/// Reads are lock-free and return one `Arc` per request, so a reload cannot change the policies of a
/// request that already started. Publication is a single atomic store: there is no window in which
/// half of a generation is visible.
#[derive(Debug, Default)]
pub struct HttpStateHandle {
    current: ArcSwapOption<HttpState>,
}

impl HttpStateHandle {
    /// Creates an empty handle: nothing published, so `current()` returns `None`.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Returns the published state, or `None` before the first successful publication.
    #[must_use]
    pub fn current(&self) -> Option<Arc<HttpState>> {
        self.current.load_full()
    }

    /// Publishes one generation: the snapshot and the policies derived from it, together.
    ///
    /// `chains` must be the plan compiled from `snapshot`'s document, and it must cover **every**
    /// route that names a plugin: a missing entry is not "no policy", it is a request that bypasses
    /// one, so the compiler that builds the plan is where coverage has to hold. This type guarantees
    /// only that the values it is given are never split across two reads.
    pub fn publish(&self, snapshot: Arc<RuntimeSnapshot>, chains: PublishedChains) {
        self.current.store(Some(Arc::new(HttpState {
            snapshot,
            policies: chains,
        })));
    }
}

/// Everything a plugin compiler produced from one document.
///
/// The four sets are the whole plan, and they are one value because publication is one step: a
/// generation that took its global chain from this document and its per-route chains from the previous
/// one would run policies the operator never put in service together.
///
/// Built by the crate that owns the plugin kinds; this crate only reads it.
pub struct PublishedChains {
    global: PluginChain,
    routes: RouteChains,
    buffered: BufferedRoutes,
    rewriting_responses: ResponseRewrites,
}

impl PublishedChains {
    /// Builds a plan from its four parts.
    #[must_use]
    pub fn new(
        global: PluginChain,
        routes: RouteChains,
        buffered: BufferedRoutes,
        rewriting_responses: ResponseRewrites,
    ) -> Self {
        Self {
            global,
            routes,
            buffered,
            rewriting_responses,
        }
    }

    /// Returns a plan that runs nothing anywhere.
    ///
    /// What a publication carries when a document declares no plugin, and what a test that only cares
    /// about routes publishes.
    #[must_use]
    pub fn empty() -> Self {
        Self::new(
            PluginChain::empty(),
            RouteChains::empty(),
            BufferedRoutes::empty(),
            ResponseRewrites::empty(),
        )
    }

    /// Returns the chain every request runs before routing.
    #[must_use]
    pub fn global(&self) -> &PluginChain {
        &self.global
    }

    /// Returns the chain each route runs after routing.
    #[must_use]
    pub fn routes(&self) -> &RouteChains {
        &self.routes
    }

    /// Returns the routes whose request body is buffered whole.
    #[must_use]
    pub fn buffered(&self) -> &BufferedRoutes {
        &self.buffered
    }

    /// Returns whether a plugin rewrites the response body of `route_id`.
    #[must_use]
    pub fn rewrites_response(&self, route_id: &str) -> bool {
        self.rewriting_responses.contains(route_id)
    }

    /// Returns the routes whose response body a plugin rewrites.
    #[must_use]
    pub fn rewriting_responses(&self) -> &ResponseRewrites {
        &self.rewriting_responses
    }
}

impl Default for PublishedChains {
    /// Returns [`PublishedChains::empty`].
    fn default() -> Self {
        Self::empty()
    }
}

impl fmt::Debug for PublishedChains {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PublishedChains")
            .field("global", &self.global)
            .field("routes", &self.routes)
            .field("buffered", &self.buffered)
            .field("rewriting_responses", &self.rewriting_responses)
            .finish()
    }
}

/// The plugin chain of every route that has one.
///
/// A route with no plugins has no entry, which is the zero-cost path: the map is empty for a
/// deployment without policies, and the lookup returns before hashing anything.
///
/// Chains are shared, not copied: [`PluginChain`] clones one `Arc`, so two routes that reference the
/// same plugins hold two pointers to one compiled chain — which is what makes interning them in the
/// pipeline worth doing.
#[derive(Clone, Default)]
pub struct RouteChains {
    chains: HashMap<String, PluginChain>,
}

impl RouteChains {
    /// Returns a chain set that runs no plugins on any route.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Returns the chain `route_id` runs, or `None` when the route has no plugins.
    #[must_use]
    pub fn chain_for(&self, route_id: &str) -> Option<&PluginChain> {
        if self.chains.is_empty() {
            return None;
        }
        self.chains.get(route_id)
    }

    /// Returns how many routes run at least one plugin.
    #[must_use]
    pub fn len(&self) -> usize {
        self.chains.len()
    }

    /// Returns whether no route runs a plugin.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.chains.is_empty()
    }
}

impl FromIterator<(String, PluginChain)> for RouteChains {
    /// Collects `(route id, chain)` pairs into a chain set.
    ///
    /// A repeated route id keeps the last pair: a document cannot express the same route twice
    /// (`Document::parse` rejects it), so a duplicate here would be a bug in the code that built the
    /// pairs rather than a configuration an operator can write.
    fn from_iter<I: IntoIterator<Item = (String, PluginChain)>>(pairs: I) -> Self {
        Self {
            chains: pairs.into_iter().collect(),
        }
    }
}

impl fmt::Debug for RouteChains {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_map().entries(&self.chains).finish()
    }
}

/// A set of route ids decided while the document was compiled.
///
/// The body and response hooks run per chunk, so they may read a decision but must not re-read
/// configuration. Both decisions this generation carries are of that shape ("which routes buffer their
/// request body", "which routes have their response body rewritten"), so one bounded set backs both —
/// and each is a distinct type, because a plan that swapped them would be a silent bug rather than a
/// compile error.
///
/// A sorted set rather than a hash set: the set has at most one id per route (bounded by
/// `SchemaBounds::max_routes`), so the lookup cost is irrelevant, while a debug line or a test that
/// lists the routes is deterministic instead of depending on hash order.
#[derive(Clone, Default)]
struct RouteIds {
    routes: BTreeSet<String>,
}

impl RouteIds {
    /// Returns whether `route_id` is in the set.
    fn contains(&self, route_id: &str) -> bool {
        self.routes.contains(route_id)
    }

    /// Returns how many routes the set holds.
    fn len(&self) -> usize {
        self.routes.len()
    }

    /// Returns whether the set is empty.
    fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }
}

impl FromIterator<String> for RouteIds {
    /// Collects the route ids the compiler decided on.
    fn from_iter<I: IntoIterator<Item = String>>(route_ids: I) -> Self {
        Self {
            routes: route_ids.into_iter().collect(),
        }
    }
}

impl fmt::Debug for RouteIds {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_set().entries(&self.routes).finish()
    }
}

/// The routes whose request body reaches plugins as one value, by route id.
///
/// Membership is decided once, while the document is compiled, from the plugins the route names: the
/// body hook runs per chunk, so it may read a decision but must not re-read configuration. A route
/// that names no plugin, or only plugins that never ask for the body, is absent — which is what keeps
/// the body on the streaming path.
#[derive(Clone, Default)]
pub struct BufferedRoutes {
    ids: RouteIds,
}

impl BufferedRoutes {
    /// Returns a set with no buffered route.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Returns whether `route_id` buffers its request body whole.
    #[must_use]
    pub fn contains(&self, route_id: &str) -> bool {
        self.ids.contains(route_id)
    }

    /// Returns how many routes buffer their request body.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// Returns whether no route buffers its request body.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
}

impl FromIterator<String> for BufferedRoutes {
    /// Collects the ids of the routes that buffer their request body.
    fn from_iter<I: IntoIterator<Item = String>>(route_ids: I) -> Self {
        Self {
            ids: route_ids.into_iter().collect(),
        }
    }
}

impl fmt::Debug for BufferedRoutes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.ids, formatter)
    }
}

/// The routes whose response body a plugin rewrites, by route id.
///
/// A route listed here cannot keep the `content-length` its upstream declared: the plugin sees each
/// chunk and may replace or drop it, so the number of bytes downstream is not the number the header
/// states. The response-header hook therefore removes it and lets the framework frame the response as
/// chunked.
///
/// Membership is per route because the decision is: a route whose chain declares
/// [`PluginPhases::RESPONSE_BODY`](crate::PluginPhases::RESPONSE_BODY) is listed, and a route whose
/// chain does not keeps its upstream framing.
#[derive(Clone, Default)]
pub struct ResponseRewrites {
    ids: RouteIds,
}

impl ResponseRewrites {
    /// Returns a set with no rewriting route.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Returns whether a plugin rewrites the response body of `route_id`.
    #[must_use]
    pub fn contains(&self, route_id: &str) -> bool {
        self.ids.contains(route_id)
    }

    /// Returns how many routes have their response body rewritten.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// Returns whether no route has its response body rewritten.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
}

impl FromIterator<String> for ResponseRewrites {
    /// Collects the ids of the routes whose response body a plugin rewrites.
    fn from_iter<I: IntoIterator<Item = String>>(route_ids: I) -> Self {
        Self {
            ids: route_ids.into_iter().collect(),
        }
    }
}

impl fmt::Debug for ResponseRewrites {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.ids, formatter)
    }
}

/// Returns the chain `route` runs, or the failure that must replace forwarding the request.
///
/// A route that names no plugin has no chain, and the caller takes the zero-cost path. A route that
/// *names* one and finds no chain must not be forwarded: the published plan would run the request
/// with no policy at all, which for an authentication plugin is an open door rather than a default.
/// The decision is a pure function of the published state and the route, so it is testable without a
/// session and every caller reaches the same answer.
///
/// # Errors
/// Returns [`HttpError::Internal`] when `route` names a plugin and the published chains do not cover
/// it. Only a compiler that skipped a route can produce that state, so it is reported as this
/// crate's own defect and answered `500` instead of being forwarded.
pub(crate) fn chain_for_route(
    state: &HttpState,
    route: &Route,
) -> Result<Option<PluginChain>, HttpError> {
    if let Some(chain) = state.chains().chain_for(route.id()) {
        return Ok(Some(chain.clone()));
    }
    if route.plugins().is_empty() {
        return Ok(None);
    }
    Err(HttpError::Internal {
        source: Box::new(io::Error::other(format!(
            "route `{}` names plugins but no plugin chain was published for it",
            route.id()
        ))),
    })
}

#[cfg(test)]
mod tests;
