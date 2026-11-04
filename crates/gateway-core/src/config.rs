//! Versioned configuration document model and its parse-time bounds.
//!
//! A document is an untrusted input boundary: it is size-checked, decoded as UTF-8, stripped of YAML
//! anchors, parsed with `deny_unknown_fields`, measured against shape bounds, and validated before
//! it can become part of a published snapshot.
//!
//! The model is split by data structure: [`Document`] and [`Limits`] describe the envelope,
//! [`Provider`], [`Upstream`] and [`Route`] describe what a route can point at, and [`ParseLimits`]
//! carries the byte and shape bounds. The serde shape, the byte-level parser and the validation
//! stages stay private: callers get [`Document::parse`] and nothing else. Reference validation
//! (does the name exist, is it unique?) and semantic validation (is the field itself legal?) are
//! separate private modules because they answer different questions.

mod bounds;
mod document;
mod error;
mod limits;
mod parse;
mod provider;
mod raw;
mod reference;
mod route;
mod semantic;
mod upstream;
mod validate;

#[cfg(test)]
mod test_support;

pub use bounds::{Bound, BoundsError, ParseLimits, SchemaBounds};
pub use document::{API_VERSION, Document};
pub use error::DocumentError;
pub use limits::Limits;
pub use provider::{Provider, ProviderKind};
pub use route::{Route, RouteTarget};
pub use upstream::{Scheme, Upstream};
