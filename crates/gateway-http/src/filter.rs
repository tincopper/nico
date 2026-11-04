//! The filter seam: HTTP policy attaches to request and response headers.
//!
//! A filter answers one question — "may this request proceed, and if not, why?" — and never writes a
//! response. Rejection is an [`HttpError`]: the data plane maps it once, so a policy module never
//! learns a status code and two filters cannot disagree about what a failed authentication returns.
//!
//! Only header stages exist. Body and stream stages are deliberately absent: the AI data plane's SSE
//! translation is per-provider state on its own path, and routing every byte of every request
//! through a generic chain would add a per-chunk dispatch to the hot path for no policy that exists
//! today.
//!
//! A filter is shared by every request of every generation (`Arc<dyn HttpFilter>`), so it is
//! stateless: per-request state belongs in [`HttpContext`], and state that outlives a request needs
//! its own synchronisation.
//!
//! The chain — not the filter — owns the budget: every stage runs under the request's remaining time
//! (`within_budget`), so a policy that awaits a remote key set cannot outlive the request it serves
//! and no filter has to remember to bound itself.

use std::{future::Future, sync::Arc, time::Instant};

use async_trait::async_trait;
use pingora::http::{RequestHeader, ResponseHeader};
use tokio::time::{Instant as TokioInstant, timeout_at};

use crate::{context::HttpContext, error::HttpError};

/// One HTTP policy stage, shared across requests.
///
/// # Contract
///
/// - A filter **may** rewrite the header it is handed; changes are forwarded.
/// - A filter **must not** write a response. It returns an error describing the rejection and the
///   data plane renders it, which keeps the status/code mapping in one place.
/// - A filter runs inline on the request's task, so anything it awaits is part of the request's
///   deadline: the chain wraps each stage in the remaining budget and reports
///   [`HttpError::DeadlineExceeded`] when it elapses.
/// - Filters run in chain order; the first error stops the chain and no later filter observes the
///   request.
#[async_trait]
pub trait HttpFilter: Send + Sync {
    /// Returns the name used in logs to attribute a decision to a filter.
    fn name(&self) -> &'static str;

    /// Inspects the request headers before an upstream is selected.
    ///
    /// # Errors
    /// Returns the failure that must become the client's response. The response is built by the
    /// data plane from the error, not by the filter.
    async fn on_request(
        &self,
        ctx: &mut HttpContext,
        request: &mut RequestHeader,
    ) -> Result<(), HttpError>;

    /// Inspects the response headers after the upstream answered.
    ///
    /// # Errors
    /// Returns the failure that must become the client's response. It is only actionable before the
    /// response is committed; after that the data plane aborts the stream instead of replacing it.
    async fn on_response(
        &self,
        ctx: &mut HttpContext,
        response: &mut ResponseHeader,
    ) -> Result<(), HttpError>;
}

/// An ordered chain of filters, shared by every request that uses it.
///
/// Order is execution order and is fixed when the chain is built, so a reload cannot reorder it
/// mid-request. Cloning the chain clones one `Arc`, not the filters.
#[derive(Clone)]
pub struct FilterChain {
    filters: Arc<[Arc<dyn HttpFilter>]>,
}

impl FilterChain {
    /// Builds a chain that runs `filters` in the given order.
    #[must_use]
    pub fn new(filters: Arc<[Arc<dyn HttpFilter>]>) -> Self {
        Self { filters }
    }

    /// Builds a chain that runs no filters.
    ///
    /// This is the chain a deployment without policies uses: running it does nothing and allocates
    /// nothing on the request path.
    #[must_use]
    pub fn empty() -> Self {
        Self::new(Arc::from(Vec::new()))
    }

    /// Runs every request stage in order, stopping at the first failure.
    ///
    /// # Errors
    /// Returns the first error a filter produced, or [`HttpError::DeadlineExceeded`] when a stage
    /// outlived the request's budget. No later filter runs either way, so the header is left as the
    /// stopping stage left it.
    pub async fn run_request(
        &self,
        ctx: &mut HttpContext,
        request: &mut RequestHeader,
    ) -> Result<(), HttpError> {
        for filter in self.filters.iter() {
            // The deadline is read before the stage borrows the context, so the stage can hold it
            // mutably for its whole life.
            let deadline = ctx.deadline();
            match within_budget(deadline, filter.on_request(ctx, request)).await {
                Ok(()) => {}
                Err(StageError::Filter(error)) => return Err(error),
                Err(StageError::Elapsed) => return Err(ctx.deadline_error()),
            }
        }
        Ok(())
    }

    /// Runs every response stage in order, stopping at the first failure.
    ///
    /// # Errors
    /// Returns the first error a filter produced, or [`HttpError::DeadlineExceeded`] when a stage
    /// outlived the request's budget. No later filter runs either way.
    pub async fn run_response(
        &self,
        ctx: &mut HttpContext,
        response: &mut ResponseHeader,
    ) -> Result<(), HttpError> {
        for filter in self.filters.iter() {
            let deadline = ctx.deadline();
            match within_budget(deadline, filter.on_response(ctx, response)).await {
                Ok(()) => {}
                Err(StageError::Filter(error)) => return Err(error),
                Err(StageError::Elapsed) => return Err(ctx.deadline_error()),
            }
        }
        Ok(())
    }
}

/// Why a stage did not finish: the filter said no, or its budget ran out.
///
/// The two are distinct because the deadline failure has to be built from the context, which the
/// stage still borrows while it runs — so the chain is the only place that can build it, after the
/// stage has been dropped.
#[derive(Debug)]
enum StageError {
    /// The filter rejected the request with this failure.
    Filter(HttpError),
    /// The request's budget ran out before the filter finished.
    Elapsed,
}

/// Runs one filter stage without letting it outlive the request's deadline.
///
/// `deadline` is the instant the request must be finished by, or `None` when it has no budget (a
/// request that never matched a route is rejected by routing, not by a clock).
async fn within_budget<F>(deadline: Option<Instant>, stage: F) -> Result<(), StageError>
where
    F: Future<Output = Result<(), HttpError>>,
{
    let Some(deadline) = deadline else {
        return stage.await.map_err(StageError::Filter);
    };
    // An absolute instant, so the stage gets exactly the time the request has left — not the whole
    // budget again, and not a second clock that could drift from the one the hooks check.
    match timeout_at(TokioInstant::from_std(deadline), stage).await {
        Ok(result) => result.map_err(StageError::Filter),
        Err(_elapsed) => Err(StageError::Elapsed),
    }
}

impl Default for FilterChain {
    /// Returns [`FilterChain::empty`].
    fn default() -> Self {
        Self::empty()
    }
}

#[cfg(test)]
mod tests;
