//! The front-ends a publication hands the data plane: one converting route, one passthrough route.
//!
//! `nico-http` reaches an LLM route through a byte-level seam ([`LlmRoute`](nico_http::llm::LlmRoute)); this module is where
//! the exchange built in [`crate::filter`] becomes that seam. Two values, one per route mode:
//!
//! | Value | Mode | What it does |
//! |---|---|---|
//! | [`ConvertRoute`] | `convert` | decodes the buffered body once, encodes the provider request, and builds one transform per response |
//! | [`PassthroughRoute`] | `passthrough` | moves both bodies unchanged; the endpoint is the whole front-end |
//!
//! A converting route serves an **attempt chain** ([`AttemptChain`]): its primary target followed by
//! the fallback members the document named, each compiled into a full exchange at publication. The
//! chain is what a retry and a fallback move across, and what a target's long-lived failure state is
//! keyed by ([`TargetIdentity`]); the data plane never sees it — the front-end still answers the
//! byte-level seam.
//!
//! Both are published values: compiled while the document is compiled, shared by every request of
//! the route, and holding no per-request state. The destination, path and headers come from the
//! adapter's endpoint, which [`ConvertRoute::new`]'s exchange resolved at publication and
//! [`PassthroughRoute::try_new`] resolves in its constructor — so the upstream head never depends on
//! the client's body.
//!
//! # Why the mode is two types
//!
//! A passthrough route has no canonical exchange to build, and a converting route has no business
//! answering bytes it did not decode. One type with an `Option<Exchange>` would let a request reach
//! the wrong half; two types make the mode a property of the published value, and the data plane asks
//! for it once through [`LlmRoute::converting`](nico_http::llm::LlmRoute::converting) instead of
//! branching on a flag beside the methods the flag governs.
//!
//! # Failure
//!
//! Building a passthrough front-end can fail — the route converts, the provider serves another kind,
//! the two sides do not speak the same protocol, or the base URL is not addressable. All four are
//! **publication** failures ([`crate::ExchangeBuildError`]): the document is refused and the
//! previously published generation keeps serving. A request-time failure is the exchange's own
//! [`HttpError`](nico_http::error::HttpError), and a refused request keeps the client codec's error
//! document — see
//! [`ConvertRoute`]'s `prepare`.

mod attempt;
mod attempt_adapter;
mod chain;
mod convert;
mod passthrough;
mod transform;

pub use attempt::{AttemptPlan, AttemptSequence, SelectionRefused, Verdict};
pub use chain::{AttemptChain, BreakerParams, ChainBuildError, ChainMember, TargetIdentity};
pub use convert::ConvertRoute;
pub use passthrough::PassthroughRoute;
