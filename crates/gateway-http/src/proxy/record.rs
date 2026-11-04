//! The request's one structured log record.
//!
//! This lives next to the hooks instead of inside them because "what an operator reads for one
//! request" is a decision of its own: the hook contract decides *when* the record is emitted, this
//! module decides *what it says and at which level*. Keeping them apart is also what keeps
//! `hooks.rs` inside the file budget.
//!
//! There is no unit test here on purpose: the subject is the text an operator reads, and the
//! end-to-end targets assert exactly that (`http_cancel.rs` for `499` + `Cancelled`,
//! `http_limits.rs` for `408` + `Client`, `http_routing.rs` for a rejected route).

use std::time::Instant;

use gateway_core::ErrorCategory;
use pingora::{Error, http::Method, proxy::Session};

use super::failure::is_downstream_gone;
use crate::context::HttpContext;

/// The log-only status for a request that was cancelled before anything was written.
///
/// `499` is nginx's convention and the value the backend error spec assigns to `Cancelled`; it is
/// never sent on the wire, only reported.
const CANCELLED_STATUS: u16 = 499;

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
    let generation = ctx.generation().map_or(0, gateway_core::Generation::get);
    let trace_id = ctx.trace_id();
    let method = ctx.method().map_or("", Method::as_str);
    let body_bytes = ctx.body_bytes();

    // A cancellation is checked first: a request whose client left, or whose deadline expired after
    // the status line was already sent, has a recorded cause but no response to report, so it
    // belongs at `debug!` rather than in the error stream.
    if cancelled {
        match ctx.error() {
            Some(failure) => tracing::debug!(
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
                "request cancelled"
            ),
            None => tracing::debug!(
                trace_id = %trace_id,
                route_id = %route_id,
                upstream = %upstream,
                config.generation = generation,
                http.method = method,
                http.status = status,
                body_bytes = body_bytes,
                elapsed_ms = elapsed_ms,
                error.category = ?ErrorCategory::Cancelled,
                "request cancelled"
            ),
        }
        return;
    }

    if let Some(failure) = ctx.error() {
        // A rejection the caller can fix is `warn!`; everything else is a failure the operator must
        // act on. Neither `info!` nor a per-request level above these is allowed on the request
        // path.
        match failure.category() {
            ErrorCategory::Client | ErrorCategory::Limit => tracing::warn!(
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
                "request rejected"
            ),
            _ => tracing::error!(
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
                "request failed"
            ),
        }
        return;
    }

    tracing::debug!(
        trace_id = %trace_id,
        route_id = %route_id,
        upstream = %upstream,
        config.generation = generation,
        http.method = method,
        http.status = status,
        body_bytes = body_bytes,
        elapsed_ms = elapsed_ms,
        "request completed"
    );
}
