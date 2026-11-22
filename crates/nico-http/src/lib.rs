//! The gateway's HTTP data plane: a Pingora integration plus the HTTP domain vocabulary.
//!
//! This crate turns a published [`RuntimeSnapshot`](nico_core::RuntimeSnapshot) into served
//! traffic. It owns the HTTP concerns of that path and nothing else:
//!
//! - [route] — which route a request matches, decided from request headers only.
//! - [plugin] — the five-phase policy contract a built-in or WASM plugin implements: the `Plugin`
//!   trait, the chain that drives it, and the registry that turns a document's `kind` into an
//!   implementation.
//! - [error] — the single failure vocabulary that maps to exactly one wire response.
//! - [context] — the per-request state the data plane keeps, including its identity and the peer the
//!   request goes to. A plugin reaches it only through `PluginContext`.
//! - [state] — the one published value the request path reads: a snapshot, the global and per-route
//!   chains compiled from it, the two per-route framing decisions (buffered request body, rewritten
//!   response body), and the LLM front-ends of the generations that have provider routes.
//! - [llm] — the byte-level seam an LLM front-end implements: where a provider route goes, how its two
//!   bodies are translated, and the per-response transform that does it.
//! - [proxy] — the Pingora hook contract that ties the above together, and [mod@service] — the
//!   process-level assembly (server configuration, listener). The same module publishes
//!   [`listener_tls`]: one inbound TLS policy, consumed by the data plane's TLS entries and by the
//!   self-reporting listener in `nico-server`, which differ only in the [`ListenerAlpn`] they ask
//!   for.
//! - [settings] — the validated `http` section (listeners, plaintext HTTP/2) and the shared
//!   listener-address rule; [server_settings] — the validated `runtime` section (threads, shutdown
//!   windows).
//!
//! Two boundaries stay inside the crate: `budget` owns the body/time budget arithmetic, and
//! `upstream` owns "destination → connectable peer". Neither is part of the public contract: the
//! only things another crate needs from the second one are [`Destination`], the value a policy hands
//! over when it understood a target this crate does not, and [`TlsPolicy`] with [`validate_bundle`]
//! and [`ensure_usable`] — the two checks a bootstrap file's trust roots go through, first while the
//! document is parsed and then again when the process is assembled, before Pingora builds its
//! connector from them. The Pingora header types are no longer re-exported either: a plugin never
//! sees one, so there is no reason for its crate to depend on the framework.
//!
//! # Boundaries
//!
//! The crate depends on `nico-core` and `pingora`, and on nothing else in the workspace. It must
//! never import `nico-llm` or `nico-plugins`: those depend on this crate, and LLM/provider
//! semantics reach the data plane the other way round — [llm] states them as a byte-level seam in which
//! a front-end answers a destination, a wire body and a per-response transform, so provider
//! configuration stays in the crate that understands it while TLS policy, budgets and header rewriting
//! stay here.
//!
//! # Invariants
//!
//! - **Route selection never reads the request body.** Pingora's body hooks stream chunks and never
//!   replay them, so a policy that consumed the body would leave the upstream waiting forever. Every
//!   decision here is made from request headers — except the body stages, which hand the chunk back.
//! - **One snapshot per request.** The context holds one `Arc<RuntimeSnapshot>` for the whole
//!   request, so a reload cannot change the route, limits or generation mid-flight.
//! - **One peer decision per request.** The destination is chosen once — by a filter that resolved
//!   the target, or from the document — and recorded, so the hook that connects and the hook that
//!   rewrites `Host` cannot describe different peers.
//! - **One deadline.** The request's budget exists before routing — a global plugin runs before a
//!   route does — and is enforced at every point that can block: each plugin stage, the connect, the
//!   response headers, the response stream and the request body stream.
//! - **One error mapping.** [`HttpError`] is the only failure vocabulary; [`HttpError::respond`] is
//!   the only place that decides a status code and a stable error code. Plugins and hooks describe
//!   *why* something failed and never build a response themselves.
//! - **Upstream semantics are not rewritten.** No response compression, no injected `server` header,
//!   no `Content-Length` changes, no retries.
//!
//! # Examples
//!
//! Route selection, the part that needs no runtime:
//!
//! ```
//! use nico_core::{Document, ParseLimits};
//! use nico_http::match_route;
//! use pingora::http::RequestHeader;
//!
//! let yaml = "\
//! apiVersion: gateway/v1
//! request:
//!   max_body_bytes: 1048576
//!   timeout_ms: 30000
//! plugins: []
//! providers: []
//! upstreams:
//!   - name: api-backend
//!     scheme: http
//!     host: 127.0.0.1
//!     port: 8080
//! routes:
//!   - id: api-v1
//!     path_prefix: /api/v1/
//!     upstream: api-backend
//! ";
//! let document = Document::parse(yaml.as_bytes(), ParseLimits::new(4096)).expect("fixture parses");
//! let mut request = RequestHeader::build("GET", b"/api/v1/items", None).expect("request builds");
//! request.insert_header("host", "example.test").expect("host header");
//!
//! let matched = match_route(&request, document.routes()).expect("a route matches");
//! assert_eq!(matched.id(), "api-v1");
//! ```
#![deny(missing_docs)]

pub mod context;
pub mod error;
pub mod inbound;
pub mod llm;
pub mod plugin;
pub mod proxy;
pub(crate) mod quota;
pub mod route;
pub mod server_settings;
pub mod service;
pub mod settings;
pub mod state;
mod trusted;

pub(crate) mod budget;
pub(crate) mod upstream;

#[cfg(test)]
mod test_support;

pub use context::{HttpContext, Identity};
pub use error::{
    BodyFormat, BodyTemplate, HttpError, LlmRejection, LlmRejectionError, MAX_LITERAL_BYTES, Part,
    Rejection, RejectionError, Respond, escape_json_string,
};
pub use inbound::{InboundTls, ServerIdentityError, validate_server_identity};
pub use plugin::{
    LogLevel, Outcome, Plugin, PluginBuildError, PluginChain, PluginContext, PluginEntry,
    PluginError, PluginFactory, PluginInfo, PluginPhase, PluginPhases, PluginRegistry,
    PluginResult, RegistryError, ResponseData, ResponseError,
};
pub use proxy::HttpApp;
pub use route::match_route;
pub use server_settings::{ServerSettings, ServerSettingsError};
pub use service::{ListenerAlpn, ServiceBuildError, listener_tls, server_conf, service};
pub use settings::{HttpSettings, HttpSettingsError, Listener, validate_listen};
pub use state::{
    BufferedRoutes, HttpState, HttpStateHandle, PublishedChains, ResponseRewrites, RouteChains,
};
pub use upstream::{
    Destination,
    dns::{
        ExpandedMembers, MemberAnswer, MemberExpansionError, MemberResolver, expand_members_with,
        resolver,
    },
    tls::{BundleError, TlsPolicy, TrustRootsError, ensure_usable, validate_bundle},
};
