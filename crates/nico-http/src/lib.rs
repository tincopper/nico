//! The gateway's HTTP data plane: a Pingora integration plus the HTTP domain vocabulary.
//!
//! This crate turns a published [`RuntimeSnapshot`](nico_core::RuntimeSnapshot) into served
//! traffic. It owns the HTTP concerns of that path and nothing else:
//!
//! - [route] — which route a request matches, decided from request headers only.
//! - [filter] — the seam HTTP policy plugs into, and [error] — the single failure vocabulary that
//!   maps to exactly one wire response.
//! - [context] — the per-request state a policy may read, including the peer the request goes to.
//! - [proxy] — the Pingora hook contract that ties the above together, and [mod@service] — the
//!   process-level assembly (server configuration, listener).
//! - [settings] — the validated bootstrap knobs this data plane is configured with.
//!
//! Two boundaries stay inside the crate: `limits` owns the body/time budget arithmetic, and
//! `upstream` owns "destination → connectable peer". Neither is part of the public contract: the
//! only thing another crate needs from the second one is [`Destination`], the value a policy hands
//! over when it understood a target this crate does not.
//!
//! # Boundaries
//!
//! The crate depends on `nico-core` and `pingora`, and on nothing else in the workspace. It must
//! never import `nico-ai` or `nico-plugins`: those depend on this crate, and AI/provider
//! semantics reach the data plane the other way round — a filter that owns a provider target
//! resolves it and records a [`Destination`] in the [`HttpContext`], so provider configuration stays
//! in the crate that understands it while TLS policy, budgets and header rewriting stay here.
//!
//! # Invariants
//!
//! - **Route selection never reads the request body.** Pingora's body hooks stream chunks and never
//!   replay them, so a filter that consumed the body would leave the upstream waiting forever.
//!   Every decision here is made from request headers.
//! - **One snapshot per request.** The context holds one `Arc<RuntimeSnapshot>` for the whole
//!   request, so a reload cannot change the route, limits or generation mid-flight.
//! - **One peer decision per request.** The destination is chosen once — by a filter that resolved
//!   the target, or from the document — and recorded, so the hook that connects and the hook that
//!   rewrites `Host` cannot describe different peers.
//! - **One deadline.** The request's budget is enforced at every point that can block: the filter
//!   chain, the connect, the response headers, the response stream and the request body stream.
//! - **One error mapping.** [`HttpError`] is the only failure vocabulary; [`HttpError::respond`] is
//!   the only place that decides a status code and a stable error code. Filters and hooks describe
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
//! limits:
//!   max_request_body_bytes: 1048576
//!   request_timeout_ms: 30000
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
pub mod filter;
pub mod proxy;
pub mod route;
pub mod service;
pub mod settings;

pub(crate) mod limits;
pub(crate) mod upstream;

#[cfg(test)]
mod test_support;

pub use context::HttpContext;
pub use error::{HttpError, Respond};
pub use filter::{FilterChain, HttpFilter};
pub use proxy::HttpApp;
pub use route::match_route;
pub use service::{server_conf, service};
pub use settings::{HttpSettings, HttpSettingsError};
pub use upstream::Destination;
