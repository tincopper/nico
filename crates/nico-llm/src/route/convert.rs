//! One converting route, as the data plane sees it.
//!
//! The route is a thin adapter over the published [`Exchange`]: it reports the facts the upstream
//! head needs (destination, path, headers), decodes the buffered client body once, and builds a
//! transform per response. It holds no per-request state itself, so one value serves every request
//! of the route.
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
    Destination, LlmRejection,
    error::HttpError,
    llm::{ConvertingRoute, LlmPrepared, LlmRequest, LlmRoute, ResponseTransform},
};

use super::transform::ConvertTransform;
use crate::{Exchange, ExchangeError, protocol::RequestFacts};

/// A route that decodes the client body into the canonical request and encodes the provider's answer
/// back.
///
/// Built once per route at publication from an [`Exchange`] that already paired the codec, the
/// adapter and the route facts. The exchange resolved the endpoint, so this front-end can answer the
/// upstream head before the client body arrives.
pub struct ConvertRoute {
    exchange: Arc<Exchange>,
}

impl ConvertRoute {
    /// Wraps the exchange a publication compiled for this route.
    ///
    /// The exchange already rejected a passthrough route and a codec/adapter that did not match the
    /// route, so this constructor has nothing left to check.
    #[must_use]
    pub const fn new(exchange: Arc<Exchange>) -> Self {
        Self { exchange }
    }

    /// Turns a failed exchange into the client protocol's error answer.
    ///
    /// The status, code, category and body are the exchange's: the canonical error owns the first
    /// three, and the client codec owns the document. Only a codec that cannot encode its own error
    /// envelope reaches the fallback, which is the gateway's defect rather than the caller's.
    fn reject(&self, error: &ExchangeError) -> HttpError {
        let canonical = error.canonical();
        let reply = match self.exchange.error_reply(error) {
            Ok(reply) => reply,
            // A codec that cannot render its own failure is a defect in this crate; the error keeps
            // the codec's diagnostic for the log and answers `500` through the data plane's envelope.
            Err(source) => return HttpError::internal(source),
        };
        match LlmRejection::new(
            reply.status(),
            canonical.code(),
            canonical.message(),
            reply.into_body(),
            canonical.category(),
        ) {
            Ok(rejection) => HttpError::LlmRejected {
                rejection: Arc::new(rejection),
            },
            // The canonical error validated its own code, message and status before the codec could
            // see it, so a refusal here means the mapping above produced something inconsistent.
            Err(source) => HttpError::internal(source),
        }
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
        self.exchange.route().protocol().as_str()
    }

    fn provider(&self) -> &str {
        self.exchange.route().provider_kind().as_str()
    }

    fn destination(&self) -> &Destination {
        self.exchange.endpoint().destination()
    }

    fn path(&self) -> &str {
        self.exchange.endpoint().path()
    }

    fn headers(&self) -> &[(String, String)] {
        self.exchange.endpoint().headers()
    }
}

impl ConvertingRoute for ConvertRoute {
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
            .exchange
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
            Arc::clone(&self.exchange),
            streaming,
            max_response_bytes,
        )))
    }
}

impl fmt::Debug for ConvertRoute {
    /// Prints the published route facts: the exchange's own `Debug` names headers and never prints
    /// their values, so a credential and a prompt have no spelling here.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConvertRoute")
            .field("exchange", &self.exchange)
            .finish()
    }
}
