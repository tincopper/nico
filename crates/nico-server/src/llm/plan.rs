//! Compiling one document's provider routes into the front-ends the request path serves.
//!
//! This is the step that turns a document into the one value `nico-http` reads for an AI route: for
//! every `RouteTarget::Provider` it resolves the client codec from the protocol registry, the adapter
//! from the provider kind, the provider entry from the document, and the credential from its reference,
//! then builds either a converting front-end (an exchange plus a per-response transform) or a
//! passthrough one (an endpoint and nothing that decodes). Everything it needs is decided here, so no
//! request re-reads the document, resolves a key or parses a base URL.
//!
//! A provider **no** route references is not read at all: a declared endpoint that serves nothing
//! cannot fail a publication it takes no part in, and a deployment may list the endpoints it intends
//! to use before a route exists.
//!
//! # What is refused, and why it is refused here
//!
//! A document whose provider route cannot be served is a **publication** failure, never a request-time
//! `500`: the previously published generation keeps serving, the operator sees which route and which
//! field, and no caller receives a prompt-shaped answer to a configuration mistake. Five kinds of
//! refusal are this module's own:
//!
//! - an unknown client protocol or provider kind, so a route never silently reaches no implementation;
//! - a mode this build does not implement, because guessing "convert" could decode bytes the operator
//!   asked to forward;
//! - a `passthrough` route whose protocol is not the adapter's own, because forwarding the client's
//!   bytes to a peer that parses something else corrupts them silently — the adapter answers its native
//!   protocol instead of the plan consulting a vendor table;
//! - a request-body or response-stage plugin on the route (or on the global chain, when the document
//!   has an AI route at all), and a plugin that asks for the whole request body: a route's body
//!   belongs to its front-end, which either buffers and prepares it (`convert`) or forwards it as it
//!   arrives (`passthrough`), so a buffering policy would hold a second copy or never run at all;
//! - an unaddressable endpoint or an unreadable credential, both of which would otherwise fail every
//!   request of the route.
//!
//! The checks run in that order because it is the order of increasing cost: the phase decision is a
//! pure read of the published chains, while a credential is I/O.
//!
//! # Logging
//!
//! Compiling logs nothing. A route that says "published" before the generation is in service is a
//! claim the publication can still retract — a later route refusing the same document, an exhausted
//! generation counter — so [`LlmPlan::log_published`] emits the line, and the publication calls it
//! after the snapshot it describes is stored. Nothing else here is logged: a base URL, a header value
//! and a credential never enter the log, and a model map's contents are the document's, not the log's.

use std::{collections::BTreeMap, fmt, sync::Arc};

use nico_core::{Document, Provider, Route, RouteMode, RouteTarget};
use nico_http::{
    PluginChain, PluginPhases, PublishedChains,
    llm::{LlmRoute, LlmRoutes},
};
use nico_llm::{
    ConvertRoute, Credential, Exchange, ExchangeMode, PassthroughRoute, ProtocolRegistry,
    ProviderRegistry, ProviderRoute,
};

use super::{CredentialResolver, LlmBuildError};

/// The stages an LLM route cannot offer a plugin, with the spelling the publication log uses.
///
/// `REQUEST` is deliberately absent: authentication and rate limiting are header decisions that run
/// before the body is buffered, so a converted route keeps them. Logging is not a phase flag at all
/// and runs as usual.
const UNSUPPORTED_PHASES: [(PluginPhases, &str); 3] = [
    (PluginPhases::REQUEST_BODY, "request_body"),
    (PluginPhases::RESPONSE, "response"),
    (PluginPhases::RESPONSE_BODY, "response_body"),
];

/// The front-ends one document published, keyed by route id.
///
/// A request looks a route up in the publication that routed it, so a reload cannot hand it another
/// generation's destination or credential. The map is read-only after publication and shared through
/// the state handle, so a request clones one pointer.
pub struct LlmPlan {
    routes: BTreeMap<String, PlannedRoute>,
}

/// One published front-end and the one fact about it only a report needs.
///
/// The model map is the document's, and `nico-http` never learns it exists: a codec maps a model name
/// while decoding, and whether a route does so at all is a property of the document. Keeping the flag
/// here rather than on [`LlmRoute`] keeps the byte seam free of a question it cannot answer.
struct PlannedRoute {
    front_end: Arc<dyn LlmRoute>,
    /// Whether the route renames models. Never the map's contents.
    renames_models: bool,
}

impl LlmPlan {
    /// Returns a plan that serves no route.
    ///
    /// What a publication carries when no route targets a provider. It is a value rather than an
    /// `Option` because the request path has one lookup and no branch a missing plan could turn into
    /// "forward the caller's bytes anyway".
    #[must_use]
    pub fn empty() -> Self {
        Self {
            routes: BTreeMap::new(),
        }
    }

    /// Compiles every provider route of `document` into a front-end.
    ///
    /// `chains` is the plugin plan compiled from the same document: the two are read together because
    /// a route's body ownership is one decision, not two. `credentials` is the process's resolver —
    /// reading a key is I/O, and a test supplies its own instead of mutating the environment.
    ///
    /// # Errors
    /// Returns [`LlmBuildError`] when a provider route cannot be served: an unknown protocol or
    /// provider kind, a mode this build does not implement, a passthrough pair that does not speak the
    /// same protocol, a route (or global) plugin stage an LLM route cannot offer, a route that is
    /// buffered twice, unusable route facts, an unreadable credential, or an endpoint the adapter
    /// cannot compile.
    pub fn compile(
        document: &Document,
        protocols: &ProtocolRegistry,
        providers: &ProviderRegistry,
        chains: &PublishedChains,
        credentials: &dyn CredentialResolver,
    ) -> Result<Self, LlmBuildError> {
        let routes: Vec<&Route> = document
            .routes()
            .iter()
            .filter(|route| matches!(route.target(), RouteTarget::Provider(_)))
            .collect();
        if routes.is_empty() {
            return Ok(Self::empty());
        }

        check_plugins(&routes, chains)?;

        let mut compiled = BTreeMap::new();
        // One materialization per provider entry: two routes of the same provider share one read, and
        // a `file:` reference is not re-opened per route.
        let mut materialized: BTreeMap<String, Credential> = BTreeMap::new();
        for route in routes {
            let planned = compile_route(
                document,
                protocols,
                providers,
                credentials,
                &mut materialized,
                route,
            )?;
            compiled.insert(route.id().to_owned(), planned);
        }
        Ok(Self { routes: compiled })
    }

    /// Returns the route ids this plan serves, in order.
    ///
    /// A publication log and a test both need "which routes are covered" without opening a front-end;
    /// the ids are the document's, so nothing about a destination or a credential is reachable here.
    pub fn route_ids(&self) -> impl Iterator<Item = &str> {
        self.routes.keys().map(String::as_str)
    }

    /// Emits one `info!` line per route, once the generation it belongs to is in service.
    ///
    /// The caller is the publication, after the snapshot store succeeds. The fields are the report an
    /// operator reads to know which routes this generation serves; nothing about a destination, a
    /// header value or a credential is reachable from here.
    pub fn log_published(&self) {
        for (route_id, planned) in &self.routes {
            tracing::info!(
                route_id = %route_id,
                llm.protocol = planned.front_end.protocol(),
                llm.provider = planned.front_end.provider(),
                llm.mode = planned.front_end.mode_name(),
                llm.model_map = planned.renames_models,
                "LLM route published"
            );
        }
    }
}

impl LlmRoutes for LlmPlan {
    fn route(&self, route_id: &str) -> Option<Arc<dyn LlmRoute>> {
        self.routes
            .get(route_id)
            .map(|planned| Arc::clone(&planned.front_end))
    }

    fn len(&self) -> usize {
        self.routes.len()
    }
}

impl fmt::Debug for LlmPlan {
    /// Prints how many routes the plan covers and nothing about them: a front-end holds a
    /// destination, a path and possibly a credential, and this value is logged once per publication.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LlmPlan")
            .field("routes", &self.routes.len())
            .finish()
    }
}

/// Builds one route's front-end, failing as a publication error.
fn compile_route(
    document: &Document,
    protocols: &ProtocolRegistry,
    providers: &ProviderRegistry,
    credentials: &dyn CredentialResolver,
    materialized: &mut BTreeMap<String, Credential>,
    route: &Route,
) -> Result<PlannedRoute, LlmBuildError> {
    let route_id = route.id();
    let protocol = route
        .protocol()
        .ok_or_else(|| LlmBuildError::MissingProtocol {
            route: route_id.to_owned(),
        })?;
    let codec = protocols
        .codec(protocol)
        .ok_or_else(|| LlmBuildError::UnknownProtocol {
            route: route_id.to_owned(),
            protocol: protocol.to_owned(),
        })?
        .clone();

    let provider_name = route.target().name();
    let entry =
        provider_entry(document, provider_name).ok_or_else(|| LlmBuildError::MissingProvider {
            route: route_id.to_owned(),
            provider: provider_name.to_owned(),
        })?;
    // The document carries an opaque token: the registry that assembles the adapters is the only
    // authority on which kinds exist, so adding a provider changes no crate above this one and an
    // unknown kind is refused here instead of while the document is parsed.
    let adapter = providers
        .provider(entry.kind())
        .ok_or_else(|| LlmBuildError::UnknownProviderKind {
            route: route_id.to_owned(),
            provider: provider_name.to_owned(),
            kind: entry.kind().to_owned(),
        })?
        .clone();

    // The mode is the last decision with no cost, so it is made before the one that reads a file: a
    // document that asked for a mode this build does not serve should be refused without opening a
    // key it would never use.
    let mode = exchange_mode(route)?;

    let credential = match materialized.get(provider_name) {
        Some(credential) => credential.clone(),
        None => {
            let credential = credentials
                .resolve(entry.credential_ref())
                .map_err(|source| LlmBuildError::Credential {
                    route: route_id.to_owned(),
                    provider: provider_name.to_owned(),
                    source,
                })?;
            materialized.insert(provider_name.to_owned(), credential.clone());
            credential
        }
    };

    // The protocol and the kind are the registries' own spellings, not the document's: the entry was
    // only a lookup key, and the compiled facts must name what will actually run. Both are read before
    // the values are moved into a constructor, so the publication log still names them afterwards.
    let protocol_id = codec.protocol();
    let provider_kind = adapter.kind();
    let facts = ProviderRoute::try_new(route_id, protocol_id, provider_kind, entry.base_url())
        .map_err(|source| LlmBuildError::Route {
            route: route_id.to_owned(),
            source,
        })?
        .with_mode(mode)
        .with_credential(credential)
        .with_model_map(entry.model_map().clone())
        .map_err(|source| LlmBuildError::Route {
            route: route_id.to_owned(),
            source,
        })?
        .with_model_allowlist(entry.model_allowlist().to_vec())
        .map_err(|source| LlmBuildError::Route {
            route: route_id.to_owned(),
            source,
        })?;
    let facts = Arc::new(facts);

    let front_end: Arc<dyn LlmRoute> = match mode {
        ExchangeMode::Convert => {
            let exchange =
                Exchange::try_new(codec, adapter, Arc::clone(&facts)).map_err(|source| {
                    LlmBuildError::Exchange {
                        route: route_id.to_owned(),
                        source,
                    }
                })?;
            Arc::new(ConvertRoute::new(Arc::new(exchange)))
        }
        ExchangeMode::Passthrough => {
            let passthrough = PassthroughRoute::try_new(Arc::clone(&facts), adapter.as_ref())
                .map_err(|source| LlmBuildError::Exchange {
                    route: route_id.to_owned(),
                    source,
                })?;
            Arc::new(passthrough)
        }
        // `exchange_mode` refuses a mode this build does not know before the facts are built, so this
        // arm is unreachable today. It exists because the mode enums are extensible: a new mode must
        // arrive with a front-end, not with a route that silently converts.
        _ => {
            return Err(LlmBuildError::UnsupportedMode {
                route: route_id.to_owned(),
                mode: route.mode(),
            });
        }
    };

    Ok(PlannedRoute {
        front_end,
        renames_models: !entry.model_map().is_empty(),
    })
}

/// Returns the document's provider entry named `name`.
fn provider_entry<'a>(document: &'a Document, name: &str) -> Option<&'a Provider> {
    document
        .providers()
        .iter()
        .find(|provider| provider.name() == name)
}

/// Translates the document's mode into the exchange's.
///
/// The document's enum is `#[non_exhaustive]`, so a mode this build has never heard of arrives here as
/// an unnamed value. Refusing it is the only honest answer: treating it as `convert` would decode a
/// body the operator asked to forward, and treating it as `passthrough` would forward a body nobody
/// encoded.
fn exchange_mode(route: &Route) -> Result<ExchangeMode, LlmBuildError> {
    match route.mode() {
        RouteMode::Convert => Ok(ExchangeMode::Convert),
        RouteMode::Passthrough => Ok(ExchangeMode::Passthrough),
        mode => Err(LlmBuildError::UnsupportedMode {
            route: route.id().to_owned(),
            mode,
        }),
    }
}

/// Refuses every policy an LLM route cannot offer: a stage that would describe the body, and a
/// request-body buffer that would compete with the front-end's own.
///
/// The global chain is checked once, and only because the document has at least one AI route: a
/// document whose routes all forward to upstreams may use any stage it likes.
fn check_plugins(routes: &[&Route], chains: &PublishedChains) -> Result<(), LlmBuildError> {
    if let Some((plugin, phase)) = unsupported_stage(chains.global()) {
        return Err(LlmBuildError::GlobalPluginPhase {
            plugin: plugin.to_owned(),
            phase,
        });
    }
    for route in routes {
        if chains.buffered().contains(route.id()) {
            return Err(LlmBuildError::BufferedRoute {
                route: route.id().to_owned(),
            });
        }
        let Some(chain) = chains.routes().chain_for(route.id()) else {
            // A route that names a policy and finds none would be published with no policy at all. The
            // plugin compiler refuses that document before this runs, so reaching it means this
            // compiler is wrong — and a wrong compiler must not publish a route it cannot check.
            if route.plugins().is_empty() {
                continue;
            }
            return Err(LlmBuildError::MissingPluginChain {
                route: route.id().to_owned(),
            });
        };
        if let Some((plugin, phase)) = unsupported_stage(chain) {
            return Err(LlmBuildError::RoutePluginPhase {
                route: route.id().to_owned(),
                plugin: plugin.to_owned(),
                phase,
            });
        }
    }
    Ok(())
}

/// Returns the first entry of `chain` that declares a stage an LLM route cannot offer.
fn unsupported_stage(chain: &PluginChain) -> Option<(&str, &'static str)> {
    for entry in chain.entries() {
        let declared = entry.phases();
        if let Some((_flag, phase)) = UNSUPPORTED_PHASES
            .iter()
            .find(|(flag, _name)| declared.contains(*flag))
        {
            return Some((entry.name(), phase));
        }
    }
    None
}
