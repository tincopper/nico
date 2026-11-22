//! The per-request walk a converting route hands the data plane.
//!
//! [`AttemptSequence`] owns every decision; this module is the thin adapter that lets the data plane
//! drive it through the byte seam. Two translations happen here and nowhere else:
//!
//! - the sequence's [`Verdict`] becomes the seam's [`AttemptVerdict`], which is the same three answers
//!   under a name this crate may change without touching `nico-http`;
//! - the sequence's failures become the [`HttpError`] the data plane writes, rendered in the *client
//!   protocol's* own error document — the same rendering `prepare` has always used.
//!
//! The 64 KiB replay cap is **not** consulted here: it is a fact about the data plane's runtime, so
//! the hooks ask `nico-http`'s own `retry_after_dispatch_allowed` and this crate never learns the
//! number.

use std::sync::Arc;

use nico_http::{
    error::HttpError,
    llm::{AttemptTarget, AttemptVerdict, LlmAttempt, LlmPrepared, LlmRequest},
};

use super::{AttemptChain, AttemptSequence, Verdict};
use crate::{ExchangeError, protocol::RequestFacts, schema::CanonicalError};

/// One request's walk over a converting route's targets.
pub(crate) struct ConvertAttempt {
    sequence: AttemptSequence,
    chain: Arc<AttemptChain>,
}

impl ConvertAttempt {
    /// Wraps the sequence a request built over its chain.
    pub(crate) fn new(chain: Arc<AttemptChain>, sequence: AttemptSequence) -> Self {
        Self { sequence, chain }
    }

    /// Turns a failed exchange into the client protocol's error answer.
    ///
    /// The chain owns the rendering; the sequence's failures only choose what it renders.
    fn reject(&self, error: &ExchangeError) -> HttpError {
        self.reject_canonical(error.canonical(), Some(error))
    }

    /// Renders one canonical error in the client's protocol.
    ///
    /// `exchange_error` is the typed failure the codec may want for its diagnostic; a refusal the
    /// sequence produced from breaker state has none, and the canonical value is the whole answer.
    fn reject_canonical(
        &self,
        canonical: CanonicalError,
        exchange_error: Option<&ExchangeError>,
    ) -> HttpError {
        self.chain.reject(canonical, exchange_error)
    }

    /// Maps one verdict onto the seam's vocabulary.
    const fn verdict(verdict: Verdict) -> AttemptVerdict {
        match verdict {
            Verdict::Answer => AttemptVerdict::Answer,
            Verdict::Exhausted => AttemptVerdict::Exhausted,
            Verdict::Retry => AttemptVerdict::Retry,
        }
    }
}

impl LlmAttempt for ConvertAttempt {
    fn select(
        &mut self,
        now: tokio::time::Instant,
        client: Option<std::net::IpAddr>,
    ) -> Result<AttemptTarget, HttpError> {
        self.sequence.select_first(now, client).map_err(|refusal| {
            let canonical = refusal.error().clone();
            self.reject_canonical(canonical, None)
        })?;
        let endpoint = self.sequence.current_endpoint();
        Ok(AttemptTarget::new(
            endpoint.destination().clone(),
            endpoint.path().to_owned(),
            endpoint.headers().to_vec(),
            self.sequence.current_identity().provider().to_owned(),
        ))
    }

    fn prepare(
        &mut self,
        request: &LlmRequest<'_>,
        _now: tokio::time::Instant,
    ) -> Result<LlmPrepared, HttpError> {
        // The decode is idempotent by contract: the sequence remembers the canonical value, so a
        // retry's call here only re-encodes for the new target.
        if !self.sequence.is_decoded() {
            let facts = RequestFacts::new(
                request.method(),
                request.path(),
                request
                    .headers()
                    .iter()
                    .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                    .collect(),
            );
            self.sequence
                .decode(&facts, request.body())
                .map_err(|error| self.reject(&error))?;
        }
        // The body is now decoded, so the model this request names is known. When no pool member can
        // serve it, the client is told so instead of being sent to a provider that would refuse:
        // this is the one place the mismatch can be detected before any attempt is encoded.
        if let Err(refusal) = self.sequence.refuse_unservable_model() {
            return Err(self.reject_canonical(refusal.error().clone(), None));
        }
        self.sequence
            .encode_current()
            .map_err(|error| self.reject(&error))?;
        let plan = self
            .sequence
            .plan()
            .ok_or_else(|| HttpError::internal("the attempt was encoded without a plan"))?;
        let streaming = plan.streaming();
        let model = plan.model();
        let label = format!("{} -> {}", model.logical(), model.physical());
        let body = plan.wire().body().to_vec();
        Ok(LlmPrepared::new(body, streaming).with_model(label))
    }

    fn classify(
        &mut self,
        status: u16,
        now: tokio::time::Instant,
    ) -> Result<AttemptVerdict, HttpError> {
        let verdict = self
            .sequence
            .classify(status, now)
            .map_err(|error| self.reject(&error))?;
        Ok(Self::verdict(verdict))
    }

    fn failed(&mut self, now: tokio::time::Instant) -> Result<AttemptVerdict, HttpError> {
        let verdict = self
            .sequence
            .transport_failed(now)
            .map_err(|error| self.reject(&error))?;
        Ok(Self::verdict(verdict))
    }

    fn succeeded(&mut self, now: tokio::time::Instant) {
        self.sequence.succeeded(now);
    }

    fn can_retry(&self) -> bool {
        self.chain.members().len() > 1 || self.sequence.budget() > 1
    }

    fn current_target(&self) -> &str {
        self.sequence.current_identity().provider()
    }

    fn streaming(&self) -> Option<bool> {
        self.sequence.streaming()
    }

    fn ttft(&self) -> Option<std::time::Duration> {
        self.sequence.ttft()
    }

    fn physical_model(&self) -> Option<String> {
        self.sequence.plan_physical_model()
    }

    fn attempt_number(&self) -> u32 {
        self.sequence.attempt_number()
    }

    fn target_view(&self) -> AttemptTarget {
        let endpoint = self.sequence.current_endpoint();
        let target = AttemptTarget::new(
            endpoint.destination().clone(),
            endpoint.path().to_owned(),
            endpoint.headers().to_vec(),
            self.sequence.current_identity().provider().to_owned(),
        );
        let target = target.with_ttft(self.sequence.ttft());
        match self.sequence.plan_model_label() {
            Some(label) => target.with_model(label),
            None => target,
        }
    }

    fn body(&self) -> Option<&[u8]> {
        self.sequence.plan_body()
    }

    fn last_refusal(&self) -> Option<HttpError> {
        self.sequence
            .last_refusal()
            .map(|canonical| self.reject_canonical(canonical, None))
    }

    fn take_skipped(&mut self) -> Vec<String> {
        self.sequence.take_skipped()
    }
}
