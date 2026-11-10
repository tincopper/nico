//! The request's one structured log record.
//!
//! This lives next to the hooks instead of inside them because "what an operator reads for one
//! request" is a decision of its own: the hook contract decides *when* the record is emitted, this
//! module decides *what it says and at which level*. Keeping them apart is also what keeps
//! `hooks.rs` inside the file budget.
//!
//! There is no unit test here on purpose: the subject is the text an operator reads, and the
//! end-to-end targets assert exactly that (`http_cancel.rs` for `499` + `Cancelled`,
//! `http_limits.rs` for `408` + `Client`, `http_routing.rs` for a rejected route, and
//! `llm_convert.rs`/`llm_passthrough.rs` for the `llm.*` fields).

use std::time::Instant;

use nico_core::ErrorCategory;
use pingora::{Error, http::Method, proxy::Session};
use tracing::Level;

use super::failure::is_downstream_gone;
use crate::context::HttpContext;

mod metrics;

/// The log-only status for a request that was cancelled before anything was written.
///
/// `499` is nginx's convention and the value the backend error spec assigns to `Cancelled`; it is
/// never sent on the wire, only reported.
const CANCELLED_STATUS: u16 = 499;

/// Emits one record, adding `authn`, `plugin` and the LLM facts only when those facts exist.
///
/// `tracing` cannot add a field conditionally inside one invocation, and writing `authn = ""` would
/// claim "authenticated by nothing" where the truth is "not authenticated" — two different facts for
/// anyone filtering the log. `plugin` is the instance a failure is attributed to, absent when no
/// plugin failed, so a 500 caused by a policy can be told apart from one caused by an upstream.
///
/// An LLM route reports its two publication facts (`llm.protocol`, `llm.provider`), and — once its
/// client body has been decoded — the model pair and the stream flag (`llm.model`, `llm.stream`).
/// A route that is not an LLM route reports none of them, and a decoded request always reports all
/// four: an empty `llm.protocol` would claim a protocol whose name is the empty string, and
/// `llm.stream = false` would claim a caller asked for a complete answer when it asked for nothing
/// this route decoded. The level stays a literal at each call site, so every expansion still has
/// static callsite metadata.
macro_rules! record {
    // A route that is not an LLM route: none of the `llm.*` fields exists.
    ($level:expr, $ctx:expr, $message:literal, $($fields:tt)*) => {
        match ($ctx.identity(), $ctx.failed_plugin()) {
            (Some(identity), Some(plugin)) => tracing::event!(
                $level, authn = %identity.source(), plugin = %plugin, $($fields)* $message
            ),
            (Some(identity), None) => tracing::event!(
                $level, authn = %identity.source(), $($fields)* $message
            ),
            (None, Some(plugin)) => tracing::event!(
                $level, plugin = %plugin, $($fields)* $message
            ),
            (None, None) => tracing::event!($level, $($fields)* $message),
        }
    };

    // An LLM route whose client body was decoded: the model pair and the stream flag as well.
    ($level:expr, $ctx:expr, llm = $llm:ident, prepared, $message:literal, $($fields:tt)*) => {
        match ($ctx.identity(), $ctx.failed_plugin()) {
            (Some(identity), Some(plugin)) => tracing::event!(
                $level,
                authn = %identity.source(),
                plugin = %plugin,
                llm.protocol = %$llm.protocol(),
                llm.provider = %$llm.provider(),
                llm.model = ?$llm.model().unwrap_or_default(),
                llm.stream = $llm.streaming().unwrap_or_default(),
                $($fields)* $message
            ),
            (Some(identity), None) => tracing::event!(
                $level,
                authn = %identity.source(),
                llm.protocol = %$llm.protocol(),
                llm.provider = %$llm.provider(),
                llm.model = ?$llm.model().unwrap_or_default(),
                llm.stream = $llm.streaming().unwrap_or_default(),
                $($fields)* $message
            ),
            (None, Some(plugin)) => tracing::event!(
                $level,
                plugin = %plugin,
                llm.protocol = %$llm.protocol(),
                llm.provider = %$llm.provider(),
                llm.model = ?$llm.model().unwrap_or_default(),
                llm.stream = $llm.streaming().unwrap_or_default(),
                $($fields)* $message
            ),
            (None, None) => tracing::event!(
                $level,
                llm.protocol = %$llm.protocol(),
                llm.provider = %$llm.provider(),
                llm.model = ?$llm.model().unwrap_or_default(),
                llm.stream = $llm.streaming().unwrap_or_default(),
                $($fields)* $message
            ),
        }
    };

    // An LLM route whose body was never decoded: a passthrough route, or a provider that answered
    // before the client finished sending. Only the two publication facts are known.
    ($level:expr, $ctx:expr, llm = $llm:ident, $message:literal, $($fields:tt)*) => {
        match ($ctx.identity(), $ctx.failed_plugin()) {
            (Some(identity), Some(plugin)) => tracing::event!(
                $level,
                authn = %identity.source(),
                plugin = %plugin,
                llm.protocol = %$llm.protocol(),
                llm.provider = %$llm.provider(),
                $($fields)* $message
            ),
            (Some(identity), None) => tracing::event!(
                $level,
                authn = %identity.source(),
                llm.protocol = %$llm.protocol(),
                llm.provider = %$llm.provider(),
                $($fields)* $message
            ),
            (None, Some(plugin)) => tracing::event!(
                $level,
                plugin = %plugin,
                llm.protocol = %$llm.protocol(),
                llm.provider = %$llm.provider(),
                $($fields)* $message
            ),
            (None, None) => tracing::event!(
                $level,
                llm.protocol = %$llm.protocol(),
                llm.provider = %$llm.provider(),
                $($fields)* $message
            ),
        }
    };
}

/// Chooses the record shape the request's facts allow.
///
/// The `llm.model` guard is the decoded-body guard: [`LlmExchange::record_prepared`] sets the model
/// label and the stream flag together, and a passthrough route never calls it. A front-end that
/// reports a stream flag without a model label is recorded with the two publication facts only — an
/// absent field is honest, and an empty one would be a value the front-end never reported.
macro_rules! emit {
    ($level:expr, $ctx:expr, $message:literal, $($fields:tt)*) => {
        match $ctx.llm() {
            Some(llm) if llm.model().is_some() => {
                record!($level, $ctx, llm = llm, prepared, $message, $($fields)*)
            }
            Some(llm) => record!($level, $ctx, llm = llm, $message, $($fields)*),
            None => record!($level, $ctx, $message, $($fields)*),
        }
    };
}

/// Emits the request's one record, at the level the operator's action implies.
///
/// The level is that action: a normal request is `debug!` (the spec forbids `info!` on the request
/// path), a rejection the caller can fix is `warn!`, and a failure the operator must act on is
/// `error!`. No body, credential or query string is logged.
pub(super) fn emit(session: &Session, error: Option<&Error>, ctx: &HttpContext) {
    let written = session
        .response_written()
        .map(|response| response.status.as_u16());
    // A request nobody answered is logged as `499`, the log-only status the error spec reserves for
    // cancellation; a request that had already written a response keeps its real status. `0` stays
    // for "nothing was written and nothing was cancelled" and for an unpublished snapshot
    // (`Generation::ZERO`), which is exactly what happened.
    let cancelled =
        ctx.is_cancelled() || (ctx.error().is_none() && error.is_some_and(is_downstream_gone));
    let status = written.unwrap_or(if cancelled { CANCELLED_STATUS } else { 0 });
    let elapsed_ms = ctx.elapsed_ms(Instant::now());
    let route_id = ctx.route_id().unwrap_or_default();
    let upstream = ctx.upstream().unwrap_or_default();
    let generation = ctx.generation().map_or(0, nico_core::Generation::get);
    let trace_id = ctx.trace_id();
    let method = ctx.method().map_or("", Method::as_str);
    let body_bytes = ctx.body_bytes();

    // The metrics are recorded once, here, from the same facts the log record below uses. The
    // call is unconditional: with no recorder installed the facade is a no-op.
    metrics::record_request(status, ctx, elapsed_ms);

    // A cancellation is checked first: a request whose client left, or whose deadline expired after
    // the status line was already sent, has a recorded cause but no response to report, so it
    // belongs at `debug!` rather than in the error stream.
    if cancelled {
        match ctx.error() {
            Some(failure) => emit!(
                Level::DEBUG,
                ctx,
                "request cancelled",
                trace_id = %trace_id,
                route_id = %route_id,
                upstream = %upstream,
                config.generation = generation,
                http.method = method,
                http.status = status,
                body_bytes = body_bytes,
                elapsed_ms = elapsed_ms,
                error.category = ?ErrorCategory::Cancelled,
                error = %failure,
            ),
            None => emit!(
                Level::DEBUG,
                ctx,
                "request cancelled",
                trace_id = %trace_id,
                route_id = %route_id,
                upstream = %upstream,
                config.generation = generation,
                http.method = method,
                http.status = status,
                body_bytes = body_bytes,
                elapsed_ms = elapsed_ms,
                error.category = ?ErrorCategory::Cancelled,
            ),
        }
        return;
    }

    if let Some(failure) = ctx.error() {
        // A rejection the caller can fix is `warn!`; everything else is a failure the operator must
        // act on. Neither `info!` nor a per-request level above these is allowed on the request
        // path.
        match failure.category() {
            ErrorCategory::Client | ErrorCategory::Limit => emit!(
                Level::WARN,
                ctx,
                "request rejected",
                trace_id = %trace_id,
                route_id = %route_id,
                upstream = %upstream,
                config.generation = generation,
                http.method = method,
                http.status = status,
                body_bytes = body_bytes,
                elapsed_ms = elapsed_ms,
                error.category = ?failure.category(),
                error = %failure,
            ),
            _ => emit!(
                Level::ERROR,
                ctx,
                "request failed",
                trace_id = %trace_id,
                route_id = %route_id,
                upstream = %upstream,
                config.generation = generation,
                http.method = method,
                http.status = status,
                body_bytes = body_bytes,
                elapsed_ms = elapsed_ms,
                error.category = ?failure.category(),
                error = %failure,
            ),
        }
        return;
    }

    emit!(
        Level::DEBUG,
        ctx,
        "request completed",
        trace_id = %trace_id,
        route_id = %route_id,
        upstream = %upstream,
        config.generation = generation,
        http.method = method,
        http.status = status,
        body_bytes = body_bytes,
        elapsed_ms = elapsed_ms,
    );
}
