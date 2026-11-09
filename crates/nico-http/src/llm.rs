//! The byte-level seam between this data plane and an LLM front-end.
//!
//! This crate owns *reaching* a peer. It does not know what a provider speaks, and it must not learn:
//! `nico-llm` depends on this crate, so the opposite direction would be a cycle. What crosses the
//! boundary is therefore bytes plus the few facts a translator needs — never a canonical LLM value, a
//! vendor name, or a credential that could reach a log line.
//!
//! Three traits, and they divide by lifetime rather than by direction:
//!
//! | Trait | Lives | Answers |
//! |---|---|---|
//! | [`LlmRoutes`] | one publication | which route id has a front-end |
//! | [`LlmRoute`] | one publication, shared by every request | where the route goes, and whether it converts at all |
//! | [`ConvertingRoute`] | one publication, reached through [`LlmRoute::converting`] | decode one body, build one transform |
//! | [`ResponseTransform`] | one response | the response half of one conversion: byte chunk in, byte chunk out |
//!
//! [`LlmRoutes`] travels inside [`HttpState`](crate::HttpState), so a request sees the front-end of the
//! generation that routed it. [`LlmRoute`] is `Send + Sync` and holds no per-request state — the
//! destination, path and headers are decided at publication, because the upstream head is written
//! before the client body is read. [`ResponseTransform`] is created per response for the same reason
//! a stream decoder is: a chunk may repeat facts (a response id, a model name) that a shared value
//! cannot remember.
//!
//! # Fail-closed
//!
//! A route that targets a provider and has no published front-end is refused with
//! [`HttpError::ProviderNotConfigured`](crate::HttpError::ProviderNotConfigured) — never forwarded with
//! the client's bytes, and never resolved against a guessed address. The publication that would
//! leave such a route has to be rejected by the crate that builds the plan; this crate only refuses to
//! serve it.
//!
//! # Two modes, and where the mode lives
//!
//! A route either **converts** or **passes bytes through**. The mode is not a flag next to the
//! translation methods: [`LlmRoute::converting`] answers `Some` only for a route that has an answer for
//! "decode this body", so "call `prepare` on a passthrough route" has no spelling. A converting route
//! holds the client body back, decodes it **once** when the stream ends, and installs a per-response
//! transform: the bytes on either side are this crate's framing, the bytes in the middle are the
//! front-end's business. A passthrough route forwards both directions byte for byte, which is the
//! escape hatch for a route whose client protocol already *is* the provider's.
//!
//! # What the data plane keeps for itself
//!
//! The destination (`Connection`, TLS policy, budgets), the request method — a chat exchange is a
//! `POST` by the client protocol's own rules, and the data plane forwards the caller's verb rather
//! than inventing one — `host`, `content-length`, `transfer-encoding` and every hop-by-hop header. A
//! front-end that names one of those in [`LlmRoute::headers`] does not get it: those headers describe a
//! message this crate frames, and a second opinion about them is how a request desynchronises from its
//! body.

mod attempt;
mod request;
mod response;
mod routes;

pub use attempt::{
    AttemptTarget, AttemptVerdict, LlmAttempt, REPLAY_BODY_LIMIT, retry_after_dispatch_allowed,
};
pub use request::{LlmPrepared, LlmRequest};
pub use response::{LlmResponseHead, ResponseTransform};
pub use routes::{ConvertingRoute, LlmRoute, LlmRoutes, NoLlmRoutes};

/// Reports whether the data plane owns `name` and will not let a front-end write it.
///
/// Three kinds are owned, for one reason each: the framing headers describe a message this crate
/// writes, the hop-by-hop headers describe a connection this crate writes, and `host` names the peer
/// this crate chose. A front-end that names one of them would be describing a message it is not
/// writing, which is how a request desynchronises from its body.
pub(crate) fn is_data_plane_owned(name: &str) -> bool {
    crate::plugin::header::is_framing(name)
        || crate::plugin::header::is_hop_by_hop(name)
        || name.eq_ignore_ascii_case("host")
}

/// Reports whether `name` negotiates a content coding rather than carrying content.
///
/// The data plane writes the bytes it forwards, so it decides the coding too: a converted answer is
/// decompressed and re-framed here, and its head is written from the front-end's own headers with the
/// provider's removed. Offering a coding this crate will not decode turns a normal answer into a parse
/// failure after the status line was already committed, so no header a front-end or a document writes
/// may reintroduce one.
pub(crate) fn is_content_coding(name: &str) -> bool {
    name.eq_ignore_ascii_case("content-encoding") || name.eq_ignore_ascii_case("accept-encoding")
}

#[cfg(test)]
mod tests;
