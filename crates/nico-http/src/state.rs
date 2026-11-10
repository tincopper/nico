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
//! A fifth fact is not a plugin's at all: the LLM front-ends one publication carries ([`LlmRoutes`]),
//! looked up by route id after routing. It travels in the same value for the same reason — a route
//! served with another generation's endpoint would send a caller's prompt to a peer the operator
//! removed.
//!
//! [`SnapshotHandle`] is still the configuration-state fact source (readiness, degradation, generation
//! for operators); it is not part of the request path, and the reload pipeline writes both in one step.
//!
//! [`SnapshotHandle`]: nico_core::SnapshotHandle

mod plan;

use std::{fmt, io, sync::Arc};

use arc_swap::ArcSwapOption;
use nico_core::{Route, RouteTarget, RuntimeSnapshot};
pub use plan::{BufferedRoutes, PublishedChains, ResponseRewrites, RouteChains};

use crate::{
    error::HttpError,
    llm::{LlmRoute, LlmRoutes},
    plugin::PluginChain,
};

/// The configuration one generation serves requests from.
///
/// Built by the reload pipeline, never by a request: the snapshot and the policies inside it describe
/// the same document, and nothing here is mutated after construction.
pub struct HttpState {
    snapshot: Arc<RuntimeSnapshot>,
    policies: PublishedChains,
    llm: Arc<dyn LlmRoutes>,
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

    /// Returns the LLM front-ends this generation published.
    ///
    /// Empty for a generation whose routes target no provider, which is the zero-cost path: one
    /// lookup that answers `None`. A route that *does* target one and finds no front-end is refused
    /// rather than forwarded, so the empty plan is never mistaken for "no translation needed".
    #[must_use]
    pub fn llm(&self) -> &dyn LlmRoutes {
        &*self.llm
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
            .field("llm_routes", &self.llm.len())
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

    /// Returns whether this process can currently serve a request.
    ///
    /// This is the named form of the `503 not_ready` check in the request hook: that branch takes
    /// the snapshot itself with `let Some(state) = self.current() else`, which is this predicate
    /// plus the value it needs, so it reads the slot once instead of asking here and reading again.
    /// The self-reporting surface asks the same question, which is why "can this process serve"
    /// has exactly one definition.
    #[must_use]
    pub fn servable(&self) -> bool {
        self.current.load_full().is_some()
    }

    /// Publishes one generation: the snapshot, the policies derived from it and its LLM front-ends.
    ///
    /// All three are one store, so a request cannot route with this generation and translate with the
    /// previous one's endpoints.
    ///
    /// `chains` must be the plan compiled from `snapshot`'s document, and it must cover **every**
    /// route that names a plugin: a missing entry is not "no policy", it is a request that bypasses
    /// one, so the compiler that builds the plan is where coverage has to hold. `llm` is the plan
    /// compiled from the same document for the same reason: what it does not cover is refused, never
    /// guessed. A document with no provider route compiles to an empty plan, not to a missing one —
    /// which is why there is no two-argument overload that supplies
    /// [`NoLlmRoutes`](crate::llm::NoLlmRoutes) for you.
    pub fn publish_with_llm(
        &self,
        snapshot: Arc<RuntimeSnapshot>,
        chains: PublishedChains,
        llm: Arc<dyn LlmRoutes>,
    ) {
        self.current.store(Some(Arc::new(HttpState {
            snapshot,
            policies: chains,
            llm,
        })));
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

/// Returns the LLM front-end `route` was published with, or the failure that must replace forwarding.
///
/// A route that targets an upstream has no front-end, and the caller takes the byte-forwarding path.
/// A route that targets a *provider* and finds none must not be forwarded: this crate cannot address
/// a provider, so the only other answer would be to send the caller's bytes somewhere it guessed. The
/// decision is a pure function of the published state and the route, so it is testable without a
/// session and every caller reaches the same answer.
///
/// # Errors
/// Returns [`HttpError::ProviderNotConfigured`] when `route` targets a provider and the published
/// plan has no front-end for it. Only a publication that skipped a route can produce that state, and
/// it is answered `502 provider_unavailable` — the same refusal a route with no filter used to get,
/// because "not published" and "not understood" are one condition from here.
pub(crate) fn llm_for_route(
    state: &HttpState,
    route: &Route,
) -> Result<Option<Arc<dyn LlmRoute>>, HttpError> {
    let RouteTarget::Provider(name) = route.target() else {
        return Ok(None);
    };
    state
        .llm()
        .route(route.id())
        .map(Some)
        .ok_or_else(|| HttpError::ProviderNotConfigured { name: name.clone() })
}

#[cfg(test)]
mod tests;
