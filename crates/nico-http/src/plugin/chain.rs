//! The plugins of one scope, driven stage by stage under the request's remaining budget.
//!
//! A chain is built once, while a document is compiled, and shared by every request that runs it:
//! cloning one is a refcount bump, and interning two routes with the same entries makes them point at
//! one slice. Its order is execution order and cannot change mid-request.
//!
//! The chain — not the plugin — owns three rules:
//!
//! - **The budget.** Every asynchronous stage runs inside the request's remaining time, so a plugin
//!   that awaits a remote key set cannot outlive the request it serves, and no plugin has to remember
//!   to bound itself. The two body stages are the exception by necessity: the response-body hook is
//!   synchronous, so the driver checks the clock around it instead of wrapping it.
//! - **Short-circuiting.** The first refusal stops the chain; a synthesised response stops it and is
//!   returned to the caller; no later entry observes the request.
//! - **Framing the stage.** Every call gets a [`ContextState`](super::context::ContextState) for
//!   exactly one plugin and one phase, carrying that entry's instance name and its
//!   publication-validated configuration text.
//!
//! A chain is shared across threads and requests, so it holds `Arc`s and no per-request state.

use std::{fmt, future::Future, sync::Arc, time::Instant};

use bytes::Bytes;
use pingora::http::{RequestHeader, ResponseHeader};
use tokio::time::{Instant as TokioInstant, timeout_at};

use super::{
    Outcome, Plugin, PluginError, PluginPhases, PluginResult, ResponseData,
    context::{ContextState, Phase},
};
use crate::{context::HttpContext, error::HttpError};

/// Why a stage did not finish: the plugin said no, or its budget ran out.
///
/// The two are distinct because the deadline failure has to be built from the context, which the
/// stage still borrows while it runs — so the chain builds it after the stage has been dropped, and
/// every hook reports the same elapsed time for the same request.
enum StageError {
    /// The plugin failed or refused the request. The conversion into the data plane's vocabulary is
    /// deliberately deferred until the chain can attach the instance that produced it.
    Plugin(PluginError),
    /// The request's budget ran out before the plugin finished.
    Elapsed,
}

/// The failure reported when a stage that cannot answer returns a synthesised response.
///
/// Only the request stage can carry one: replacing an upstream response would mean intercepting a
/// body that has already been announced, which this milestone deliberately does not do. It is a
/// plugin bug (the plugin answered a question nobody asked), so it is reported as `500` with the
/// boundary named rather than silently ignored.
const MISPLACED_RESPONSE: &str = "a synthesised response is only carried from the request stage; this stage cannot replace the \
     upstream response";

/// One compiled plugin instance inside a chain.
///
/// The instance name and the configuration text are computed once, at publication: a stage then costs
/// a refcount bump per call instead of an allocation per request, and the plugin never re-parses the
/// document it was built from. Cloning an entry clones three `Arc`s: the plugin instance inside it is
/// shared, never rebuilt.
#[derive(Clone)]
pub struct PluginEntry {
    plugin: Arc<dyn Plugin>,
    instance: Arc<str>,
    config: Arc<str>,
    phases: PluginPhases,
}

impl PluginEntry {
    /// Builds one chain element over `plugin`, whose context reports `config` as its configuration.
    ///
    /// The declared phases are read from [`Plugin::info`] once, here, so "what does this plugin say
    /// about itself" has one answer per compiled entry instead of one per request.
    #[must_use]
    pub fn new(plugin: Arc<dyn Plugin>, config: Arc<str>) -> Self {
        let instance: Arc<str> = Arc::from(plugin.name());
        let phases = plugin.info().phases();
        Self {
            plugin,
            instance,
            config,
            phases,
        }
    }

    /// Returns the instance name the document gave this entry.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.instance
    }

    /// Returns whether two entries drive the same plugin instance.
    ///
    /// Two chains that name the same document entry share one instance — one bucket table, one
    /// compiled key set — and this is how interning is asserted without exposing the plugin itself.
    #[must_use]
    pub fn is_same_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.plugin, &other.plugin)
    }

    /// Returns the stages this entry declares, as the plugin reported them at publication.
    #[must_use]
    pub const fn phases(&self) -> PluginPhases {
        self.phases
    }

    /// Returns whether this entry declares that it rewrites the response body.
    ///
    /// A route whose chain holds one such entry cannot keep the upstream's `content-length`, so the
    /// publication records the route and the response hook frames it as chunked.
    #[must_use]
    pub fn rewrites_response_body(&self) -> bool {
        self.phases.contains(PluginPhases::RESPONSE_BODY)
    }

    /// Builds the context of a request-side stage for this entry.
    fn request_state<'a>(
        &'a self,
        phase: Phase,
        ctx: &'a mut HttpContext,
        request: &'a mut RequestHeader,
    ) -> ContextState<'a> {
        ContextState::new(
            phase,
            ctx,
            request,
            Arc::clone(&self.instance),
            Arc::clone(&self.config),
        )
    }

    /// Builds the context of a response stage that can still change the head.
    fn response_state<'a>(
        &'a self,
        ctx: &'a mut HttpContext,
        response: &'a mut ResponseHeader,
    ) -> ContextState<'a> {
        ContextState::for_response(
            Phase::Response,
            ctx,
            response,
            Arc::clone(&self.instance),
            Arc::clone(&self.config),
        )
    }

    /// Builds a response-side context with no response attached.
    ///
    /// The stage still runs — a request that never got a response still reaches `on_logging` — and every
    /// response read then answers `PhaseViolation` instead of inventing a status.
    fn detached_state<'a>(&'a self, phase: Phase, ctx: &'a mut HttpContext) -> ContextState<'a> {
        ContextState::detached(
            phase,
            ctx,
            Arc::clone(&self.instance),
            Arc::clone(&self.config),
        )
    }

    /// Builds the context of a stage that runs after the response head was written.
    fn sent_state<'a>(
        &'a self,
        phase: Phase,
        ctx: &'a mut HttpContext,
        response: &'a ResponseHeader,
    ) -> ContextState<'a> {
        ContextState::sent(
            phase,
            ctx,
            response,
            Arc::clone(&self.instance),
            Arc::clone(&self.config),
        )
    }
}

impl fmt::Debug for PluginEntry {
    /// Prints the instance name and the declared phases, never the configuration: a compiled entry is
    /// what a diagnostic names, and the configuration text may hold a credential reference.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PluginEntry")
            .field("name", &self.name())
            .field("phases", &self.phases)
            .finish()
    }
}

/// An ordered chain of plugins, shared by every request that uses it.
///
/// Order is execution order and is fixed when the chain is built. Both directions use the same slice:
/// the request stages run it forwards, and the response and logging stages run it forwards within one
/// scope while the *scopes* run in opposite order (route before global).
#[derive(Clone)]
pub struct PluginChain {
    entries: Arc<[PluginEntry]>,
}

impl PluginChain {
    /// Builds a chain that runs `entries` in the given order.
    #[must_use]
    pub fn new(entries: Arc<[PluginEntry]>) -> Self {
        Self { entries }
    }

    /// Builds a chain that runs no plugin.
    ///
    /// This is the chain a deployment without policies uses: every stage returns immediately and
    /// allocates nothing on the request path.
    #[must_use]
    pub fn empty() -> Self {
        Self::new(Arc::from(Vec::new()))
    }

    /// Returns the entries in execution order.
    #[must_use]
    pub fn entries(&self) -> &[PluginEntry] {
        &self.entries
    }

    /// Returns how many plugins run.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether no plugin runs.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns whether any entry declares that it rewrites the response body.
    ///
    /// This is the only phase declaration with a consequence outside the plugin: it is what makes a
    /// length-framed response impossible to honour.
    #[must_use]
    pub fn rewrites_response_body(&self) -> bool {
        self.entries.iter().any(PluginEntry::rewrites_response_body)
    }

    /// Converts one plugin failure and records the instance that produced it.
    ///
    /// The chain is the only place that knows both the entry's name and the error, so attribution
    /// belongs here rather than in the plugin (which only knows itself) or the hook (which only sees
    /// the mapped [`HttpError`]). Every stage's error arm goes through this, so the one
    /// `From<PluginError> for HttpError` stays the only conversion point and no failure can escape
    /// without a `plugin` field on the request record.
    fn failure(ctx: &mut HttpContext, entry: &PluginEntry, error: PluginError) -> HttpError {
        ctx.set_failed_plugin(entry.name());
        error.into()
    }

    /// Runs every request stage in order, stopping at the first refusal or synthesised response.
    ///
    /// # Errors
    /// Returns the first failure a plugin produced, or [`HttpError::DeadlineExceeded`] when a stage
    /// outlived the request's budget. No later plugin runs either way, so the request is left as the
    /// stopping stage left it.
    pub async fn run_request(
        &self,
        ctx: &mut HttpContext,
        request: &mut RequestHeader,
    ) -> Result<Option<ResponseData>, HttpError> {
        for entry in self.entries.iter() {
            let deadline = ctx.deadline();
            let stage = {
                let mut state = entry.request_state(Phase::Request, ctx, request);
                within_budget(deadline, entry.plugin.on_request(&mut state)).await
            };
            match stage {
                Ok(Outcome::Continue) => {}
                Ok(Outcome::Respond(data)) => return Ok(Some(data)),
                Err(StageError::Plugin(error)) => return Err(Self::failure(ctx, entry, error)),
                Err(StageError::Elapsed) => return Err(ctx.deadline_error()),
            }
        }
        Ok(None)
    }

    /// Runs every request-body stage in order, returning the bytes to forward.
    ///
    /// `chunk` is one received chunk in the streaming mode, or the whole body in the buffered mode, in
    /// which case `complete` is always true. A plugin replaces it through
    /// [`PluginContext::set_request_body`](super::PluginContext::set_request_body); what this returns
    /// is what the upstream receives.
    ///
    /// # Errors
    /// As [`PluginChain::run_request`].
    pub async fn run_request_body(
        &self,
        ctx: &mut HttpContext,
        request: &mut RequestHeader,
        chunk: Vec<u8>,
        complete: bool,
    ) -> Result<Vec<u8>, HttpError> {
        let mut current = chunk;
        for entry in self.entries.iter() {
            let deadline = ctx.deadline();
            let stage = {
                let mut state = entry
                    .request_state(Phase::RequestBody, ctx, request)
                    .with_body(std::mem::take(&mut current), complete);
                // The body is read back out of the context after the call: a plugin rewrites through
                // `set_request_body`, and the value it left there is the one that travels on.
                within_budget(deadline, entry.plugin.on_request_body(&mut state))
                    .await
                    .map(|outcome| (outcome, state.take_body()))
            };
            match stage {
                Ok((Outcome::Continue, body)) => current = body,
                Ok((Outcome::Respond(_), _)) => {
                    return Err(Self::failure(ctx, entry, misplaced_response()));
                }
                Err(StageError::Plugin(error)) => return Err(Self::failure(ctx, entry, error)),
                Err(StageError::Elapsed) => return Err(ctx.deadline_error()),
            }
        }
        Ok(current)
    }

    /// Runs every response stage in order, stopping at the first failure.
    ///
    /// # Errors
    /// As [`PluginChain::run_request`]. A synthesised response is refused here: it is only carried
    /// from the request stage, and the response this stage runs against came from the upstream.
    pub async fn run_response(
        &self,
        ctx: &mut HttpContext,
        response: &mut ResponseHeader,
    ) -> Result<(), HttpError> {
        for entry in self.entries.iter() {
            let deadline = ctx.deadline();
            let stage = {
                let mut state = entry.response_state(ctx, response);
                within_budget(deadline, entry.plugin.on_response(&mut state)).await
            };
            match stage {
                Ok(Outcome::Continue) => {}
                Ok(Outcome::Respond(_)) => {
                    return Err(Self::failure(ctx, entry, misplaced_response()));
                }
                Err(StageError::Plugin(error)) => return Err(Self::failure(ctx, entry, error)),
                Err(StageError::Elapsed) => return Err(ctx.deadline_error()),
            }
        }
        Ok(())
    }

    /// Runs every response-body stage in order, and returns what to send for this chunk.
    ///
    /// `None` means "send nothing for this chunk": a plugin emptied the bytes it was handed, so there
    /// is nothing left to forward. The response is only framed as chunked when the publication recorded
    /// the rewrite, so a dropped chunk can never contradict a declared length.
    ///
    /// The stage is synchronous because the framework hook that delivers a downstream chunk cannot
    /// await. The caller checks the request's budget before and after it; a plugin must not block
    /// here.
    ///
    /// `response` is the head as written downstream. It is optional because the hook cannot prove the
    /// write happened before the first chunk arrived; without it the stage still runs — a plugin's
    /// rewrite must never be skipped — and asking for a status becomes a
    /// [`PluginError::PhaseViolation`](super::PluginError::PhaseViolation) instead of a wrong answer.
    ///
    /// # Errors
    /// Returns the first failure a plugin produced. The response head is already downstream by the
    /// time this runs, so the caller ends the stream instead of answering.
    pub fn run_response_body(
        &self,
        ctx: &mut HttpContext,
        response: Option<&ResponseHeader>,
        chunk: Bytes,
        complete: bool,
    ) -> Result<Option<Bytes>, HttpError> {
        if self.entries.is_empty() {
            return Ok(Some(chunk));
        }
        // PERF: one copy per chunk when the chain is non-empty. The contract hands a plugin an owned
        // `Vec<u8>` — a guest module has no borrow to give — so the chunk is copied into the context
        // and back out. A chain with no plugin never reaches this branch.
        let mut current = chunk.to_vec();
        for entry in self.entries.iter() {
            // The context borrows `ctx` only for the duration of the call; dropping it before the
            // error arms is what lets those arms record the instance on the same context.
            let stepped = {
                let mut state = match response {
                    Some(response) => entry.sent_state(Phase::ResponseBody, ctx, response),
                    None => entry.detached_state(Phase::ResponseBody, ctx),
                }
                .with_body(std::mem::take(&mut current), complete);
                let outcome = entry.plugin.on_response_body(&mut state);
                outcome.map(|outcome| (outcome, state.take_body()))
            };
            match stepped {
                Ok((Outcome::Continue, body)) => current = body,
                Ok((Outcome::Respond(_), _)) => {
                    return Err(Self::failure(ctx, entry, misplaced_response()));
                }
                Err(error) => return Err(Self::failure(ctx, entry, error)),
            }
        }
        Ok(if current.is_empty() {
            None
        } else {
            Some(Bytes::from(current))
        })
    }

    /// Runs every logging stage in order.
    ///
    /// This stage cannot change anything, so nothing is returned: a plugin reports what only it knows
    /// and the data plane writes the request's one record afterwards. A stage that outlives the
    /// request's budget is abandoned rather than allowed to hold the connection open, and the fact is
    /// logged under the plugin's own name.
    ///
    /// The stages run inside the request's remaining budget rather than under a fresh one: the request
    /// is over, so there is no time left to give.
    pub async fn run_logging(&self, ctx: &mut HttpContext, response: Option<&ResponseHeader>) {
        for entry in self.entries.iter() {
            let deadline = ctx.deadline();
            let mut state = match response {
                Some(response) => entry.sent_state(Phase::Logging, ctx, response),
                // Without a response the stage still runs: `status` and `response_header` then answer
                // `PhaseViolation`, which is the truth for a request that never got one.
                None => entry.detached_state(Phase::Logging, ctx),
            };
            let stage = entry.plugin.on_logging(&mut state);
            let Some(deadline) = deadline else {
                stage.await;
                continue;
            };
            if timeout_at(TokioInstant::from_std(deadline), stage)
                .await
                .is_err()
            {
                tracing::warn!(
                    plugin = entry.name(),
                    "the logging stage outlived the request budget and was abandoned"
                );
            }
        }
    }
}

/// Returns the error reported by a stage that answered where it may not.
///
/// It is a `PluginError` so it travels through [`PluginChain::failure`] like every other plugin
/// failure: one conversion, one attribution, no special case in the logging path.
fn misplaced_response() -> PluginError {
    PluginError::Execute(MISPLACED_RESPONSE)
}

/// Runs one asynchronous plugin stage without letting it outlive the request's deadline.
///
/// `deadline` is the instant the request must be finished by, or `None` when it has no budget.
async fn within_budget<F>(deadline: Option<Instant>, stage: F) -> Result<Outcome, StageError>
where
    F: Future<Output = PluginResult>,
{
    let Some(deadline) = deadline else {
        return stage.await.map_err(StageError::Plugin);
    };
    // An absolute instant, so the stage gets exactly the time the request has left — not the whole
    // budget again, and not a second clock that could drift from the one the hooks check.
    match timeout_at(TokioInstant::from_std(deadline), stage).await {
        Ok(result) => result.map_err(StageError::Plugin),
        Err(_elapsed) => Err(StageError::Elapsed),
    }
}

impl fmt::Debug for PluginChain {
    /// Prints the instance names rather than the plugins' state.
    ///
    /// A chain is shared by every request of every generation, so its contents are what a diagnostic
    /// needs; per-request state lives in [`HttpContext`] instead.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_list()
            .entries(self.entries.iter().map(PluginEntry::name))
            .finish()
    }
}

impl Default for PluginChain {
    /// Returns [`PluginChain::empty`].
    fn default() -> Self {
        Self::empty()
    }
}

#[cfg(test)]
mod tests;
