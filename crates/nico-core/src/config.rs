//! Versioned configuration document model and its parse-time bounds.
//!
//! A document is an untrusted input boundary: it is size-checked, decoded as UTF-8, stripped of YAML
//! anchors, parsed with `deny_unknown_fields`, measured against shape bounds, and validated before
//! it can become part of a published snapshot.
//!
//! The model is split by data structure: [`Document`] and [`RequestBounds`] describe the envelope,
//! [`Provider`], [`Upstream`], [`PluginSpec`] and [`Route`] describe what a route can point at and
//! run, and [`ParseLimits`] carries the byte and shape bounds. The serde shape, the byte-level parser
//! and the validation stages stay private: callers get [`Document::parse`] and nothing else.
//! Reference validation (does the name exist, is it unique?) and semantic validation (is the field
//! itself legal?) are separate private modules because they answer different questions.

mod bounds;
mod document;
mod error;
mod host;
mod parse;
mod plugin;
mod provider;
mod raw;
mod reference;
mod request;
mod route;
mod secret_ref;
mod semantic;
mod upstream;
mod validate;

#[cfg(test)]
mod test_support;

pub use bounds::{Bound, BoundsError, ParseLimits, SchemaBounds};
pub use document::{API_VERSION, Document};
pub use error::DocumentError;
pub use host::{HostError, MAX_HOST_BYTES, host_key, hosts_match, validate_host};
pub use plugin::{
    BodyValue, DEFAULT_CONTENT_TYPE, ErrorBody, ErrorResponse, PluginSpec, is_json_content_type,
};
pub use provider::{CircuitBreaker, DEFAULT_COOLDOWN_MS, DEFAULT_FAILURE_THRESHOLD, Provider};
pub use request::{DEFAULT_MAX_RESPONSE_BYTES, RequestBounds};
pub use route::{Route, RouteMode, RouteTarget};
pub use secret_ref::is_secret_ref;
pub use semantic::{MAX_ATTEMPTS_PER_ROUTE, MAX_FALLBACK_MEMBERS};
pub use upstream::{Scheme, Upstream};
