//! One converting route, as the data plane sees it.
//!
//! The route is a thin adapter over the published [`AttemptChain`]: it reports the facts the
//! upstream head needs (destination, path, headers), decodes the buffered client body once, and
//! builds a transform per response. It holds no per-request state itself, so one value serves every
//! request of the route. Today every answer comes from the chain's primary target — the attempt
//! sequence across the chain's members is the data plane's, which drives it through the byte seam.
//!
//! # The failure a caller sees
//!
//! `prepare` is the only place a request can be refused inside this module, and the refusal keeps
//! the *client protocol's* error document: the exchange maps its failure into a canonical error, the
//! client codec encodes that error in the protocol it decoded, and the result becomes an
//! [`HttpError::LlmRejected`]. Re-encoding it here as this data plane's generic envelope would tell a
//! client that speaks the provider's own protocol to parse a shape it does not know — and would lose
//! the `422` for a request the route legitimately cannot serve.
//!
//! A codec that cannot encode its own error document is a front-end defect, so the fallback is
//! `500`: the failure must not be silently reduced to the generic envelope either, because that
//! would report a refusal whose reason nobody can see.

use std::{fmt, sync::Arc};

use nico_http::{
    Destination,
    error::HttpError,
    llm::{ConvertingRoute, LlmPrepared, LlmRequest, LlmRoute, ResponseTransform},
};

use super::{attempt_adapter::ConvertAttempt, transform::ConvertTransform};
use crate::{AttemptChain, AttemptSequence, BreakerTable, ExchangeError, protocol::RequestFacts};

/// A route that decodes the client body into the canonical request and encodes the provider's answer
/// back.
///
/// Built once per route at publication from an [`AttemptChain`] whose members already paired the
/// codec, the adapters and the route facts. The chain resolved every endpoint, so this front-end can
/// answer the upstream head before the client body arrives.
pub struct ConvertRoute {
    chain: Arc<AttemptChain>,
    breakers: Arc<BreakerTable>,
    budget: u8,
}

impl ConvertRoute {
    /// Wraps the attempt chain a publication compiled for this route.
    ///
    /// The chain already rejected a passthrough route and codec/adapter pairs that did not match the
    /// route, so this constructor has nothing left to check. `breakers` is the process-wide table —
    /// the walk reads and writes the same state every generation shares, and `budget` is the route's
    /// own retry budget.
    #[must_use]
    pub fn new(chain: Arc<AttemptChain>, breakers: Arc<BreakerTable>, budget: u8) -> Self {
        Self {
            chain,
            breakers,
            budget,
        }
    }

    /// Turns a failed exchange into the client protocol's error answer.
    ///
    /// The chain owns the rendering; the exchange's failures only choose what it renders.
    fn reject(&self, error: &ExchangeError) -> HttpError {
        self.chain.reject(error.canonical(), Some(error))
    }
}

impl LlmRoute for ConvertRoute {
    fn converting(&self) -> Option<&dyn ConvertingRoute> {
        Some(self)
    }

    fn mode_name(&self) -> &'static str {
        "convert"
    }

    fn protocol(&self) -> &str {
        self.chain.primary().exchange().route().protocol().as_str()
    }

    fn provider(&self) -> &str {
        self.chain
            .primary()
            .exchange()
            .route()
            .provider_kind()
            .as_str()
    }

    fn destination(&self) -> &Destination {
        self.chain.primary().exchange().endpoint().destination()
    }

    fn path(&self) -> &str {
        self.chain.primary().exchange().endpoint().path()
    }

    fn headers(&self) -> &[(String, String)] {
        self.chain.primary().exchange().endpoint().headers()
    }
}

impl ConvertingRoute for ConvertRoute {
    fn begin(&self) -> Box<dyn nico_http::llm::LlmAttempt> {
        // The walk starts undecoded and unselected: the data plane asks for a target before the
        // client's body ends, and for the encoded body once it has.
        let sequence = AttemptSequence::new(
            Arc::clone(&self.chain),
            Arc::clone(&self.breakers),
            self.budget,
        );
        Box::new(ConvertAttempt::new(Arc::clone(&self.chain), sequence))
    }

    fn prepare(&self, request: &LlmRequest<'_>) -> Result<LlmPrepared, HttpError> {
        // The codec reads owned facts, so the view is copied into them once per converted request.
        // Only headers whose value was UTF-8 reach this point, which is what a text protocol needs.
        let facts = RequestFacts::new(
            request.method(),
            request.path(),
            request
                .headers()
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
        );
        let prepared = self
            .chain
            .primary()
            .exchange()
            .prepare(&facts, request.body())
            .map_err(|error| self.reject(&error))?;
        // Read what the record needs before the wire request is taken: the label names both models,
        // and neither name is parsed back out of it.
        let streaming = prepared.request().stream();
        let model = prepared.model();
        let label = format!("{} -> {}", model.logical(), model.physical());
        Ok(LlmPrepared::new(prepared.into_wire().into_body(), streaming).with_model(label))
    }

    fn transform(
        &self,
        streaming: bool,
        max_response_bytes: u32,
    ) -> Result<Box<dyn ResponseTransform>, HttpError> {
        Ok(Box::new(ConvertTransform::new(
            self.chain.primary().shared_exchange(),
            streaming,
            max_response_bytes,
        )))
    }
}

impl fmt::Debug for ConvertRoute {
    /// Prints the published route facts: the chain's own `Debug` reports the target count and the
    /// members name headers by name and never print their values, so a credential and a prompt have
    /// no spelling here.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConvertRoute")
            .field("chain", &self.chain)
            .finish()
    }
}
