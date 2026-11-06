//! The published front-ends: which route id has one, and what one route needs to know.

use std::sync::Arc;

use super::{LlmPrepared, LlmRequest, ResponseTransform};
use crate::{error::HttpError, upstream::Destination};

/// The LLM front-ends one publication carries, keyed by route id.
///
/// Built by the crate that owns the codecs and adapters while the document is compiled, and stored
/// inside [`HttpState`](crate::HttpState) — so a request looks a route up in the generation that
/// routed it and a reload cannot hand it another generation's endpoint.
///
/// A lookup that misses is not "no policy": a provider route without a front-end is refused, because
/// this crate cannot address a provider and forwarding the caller's bytes somewhere else would be
/// worse than failing.
pub trait LlmRoutes: Send + Sync {
    /// Returns the front-end published for `route_id`, or `None` when the plan has none.
    fn route(&self, route_id: &str) -> Option<Arc<dyn LlmRoute>>;

    /// Returns how many routes the plan serves.
    ///
    /// A count, never the plan itself: a front-end holds a destination, a path and possibly a
    /// credential, so the published state's `Debug` reports how many routes it covers and nothing
    /// about their contents.
    fn len(&self) -> usize;

    /// Returns whether the plan serves no route.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One route's front-end: where it goes, and how its two bodies are translated.
///
/// The value is shared by every request of the route and holds no per-request state, so the
/// destination, path and headers are decided once at publication. That is not an optimisation: the
/// upstream head is written before the client body is read, so anything derived from the body could
/// not be part of it.
pub trait LlmRoute: Send + Sync {
    /// Returns the exchange this route translates with, or `None` when it forwards bytes unchanged.
    ///
    /// The mode is not a flag beside the methods it governs: a passthrough route has no answer for
    /// "decode this body", so that question is only askable of a value this returns `Some` for. The
    /// data plane branches once, here, and the illegal call has no spelling left to write.
    fn converting(&self) -> Option<&dyn ConvertingRoute>;

    /// Returns the stable lowercase spelling of the mode, for logs and tests.
    ///
    /// Reported, never matched on — the branch is [`Self::converting`]. A name that is not the same
    /// value as the decision would let the two drift, so the route that decides also names.
    fn mode_name(&self) -> &'static str;

    /// Returns the client protocol this route serves, for example `vendor.surface`.
    ///
    /// A name from the front-end's registry: this crate records it and never matches on it.
    fn protocol(&self) -> &str;

    /// Returns the provider kind this route speaks to, for example `vendor`.
    ///
    /// Reported, never matched on: the adapter that understands it is behind this trait.
    fn provider(&self) -> &str;

    /// Returns the peer the upstream request is sent to.
    fn destination(&self) -> &Destination;

    /// Returns the absolute request path the upstream sees.
    fn path(&self) -> &str;

    /// Returns the headers the provider adds to the request, in the order they are written.
    ///
    /// The data plane writes them over the client's, so a provider credential replaces the caller's.
    /// A name this crate frames or forwards a message with (`host`, `content-length`,
    /// `transfer-encoding`, a hop-by-hop header) is ignored, and so is a content coding: those
    /// describe a message the data plane owns, and a second opinion about them is how a request
    /// desynchronises from its body.
    fn headers(&self) -> &[(String, String)];
}

/// The half of a front-end that only a converting route has.
///
/// Reached through [`LlmRoute::converting`], so a caller holds one only after asking for it. Every
/// method here is one this crate would have to answer with `500` if a passthrough route could be asked
/// it, which is why the question is a type rather than a condition.
pub trait ConvertingRoute: Send + Sync {
    /// Decodes the buffered client body and returns the bytes to send to the provider.
    ///
    /// Called exactly once per converted request, after the client body has ended. This is where the
    /// capability gate, the model mapping and the provider's encoder run: the returned value is what
    /// the data plane writes, so a failure here is answered before anything reaches the provider.
    ///
    /// # Errors
    /// Returns the failure that replaces the provider request. An [`HttpError::InvalidRequest`] is the
    /// caller's document, a `422` is a request this route cannot serve, and a `500` is this side's
    /// defect — the data plane maps whatever is returned and never invents a status of its own.
    fn prepare(&self, request: &LlmRequest<'_>) -> Result<LlmPrepared, HttpError>;

    /// Creates the transform for one response.
    ///
    /// `streaming` is the flag the client's own request carried, and `max_response_bytes` is the
    /// ceiling the route and the document agree on for a buffered answer. Both are inputs to the
    /// front-end's decision to buffer or to translate chunk by chunk; the data plane does not make
    /// that decision, because only the front-end knows whether a response is an event stream.
    ///
    /// `streaming` is false when the client's body was never decoded — a provider may answer before
    /// the caller has finished sending (a credential it can refuse from the head alone) — because no
    /// stream begins before its request ends. Such an answer is a refusal, and a buffered transform is
    /// what renders one.
    ///
    /// # Errors
    /// Returns the failure that replaces the response. Created before the head is written, so a
    /// front-end that cannot serve the response answers with a client error instead of half a body.
    fn transform(
        &self,
        streaming: bool,
        max_response_bytes: u32,
    ) -> Result<Box<dyn ResponseTransform>, HttpError>;
}

/// A plan that serves no route.
///
/// What a publication carries when no route targets a provider. It is a value rather than an
/// `Option<Arc<dyn LlmRoutes>>` so the request path has one lookup and no branch that a missing plan
/// could turn into "forward the caller's bytes anyway".
#[derive(Debug, Clone, Copy, Default)]
pub struct NoLlmRoutes;

impl LlmRoutes for NoLlmRoutes {
    fn route(&self, _route_id: &str) -> Option<Arc<dyn LlmRoute>> {
        None
    }

    fn len(&self) -> usize {
        0
    }
}
