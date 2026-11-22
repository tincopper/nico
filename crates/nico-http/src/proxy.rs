//! The HTTP data plane: one type, one hook contract, one response per failure.
//!
//! [`HttpApp`] is what Pingora drives. It holds the publication handle the reload pipeline writes
//! and the trace-id counter — and no per-request state, which lives
//! in [`HttpContext`] instead so two concurrent requests cannot observe each other. The plugin chains — one
//! that runs before routing, one per route — travel *inside* the published value
//! ([`crate::HttpState`]), so a request cannot see one generation's routes with another's policies.
//!
//! The hooks themselves live in `proxy/hooks.rs`, because the hook order is the part of this crate
//! that a Pingora upgrade can invalidate and it is easier to re-verify in one file; the failure
//! vocabulary they share is in `proxy/failure.rs`, the header-forwarding and destination rules in
//! `proxy/forward.rs`, and the one request log record in `proxy/record.rs`. This module keeps the
//! type itself, the request's correlation id and the one way a request is rejected.
//!
//! Three facts about Pingora 0.9.1 shape the whole module, and each of them would otherwise be
//! discovered by a hang or a corrupt response:
//!
//! - **A hook that consumes a request-body chunk never gives it back.** Phase-0 evidence: the body
//!   arrives in chunks with unstable boundaries and is not replayed, so a route decision made from
//!   the body would leave the upstream waiting forever. Every decision here uses headers only, and a
//!   body stage hands its chunk back through the context instead of consuming it.
//! - **`error_while_proxy` is synchronous.** It cannot write a response, so it is used only to
//!   classify a failure; the response is written by `fail_to_proxy`, which is asynchronous and runs
//!   exactly once for the final failure of a request. `upstream_response_body_filter` is synchronous
//!   too, which is why `Plugin::on_response_body` is the one stage a plugin cannot await in.
//! - **Informational responses reach the response hooks.** A `100 Continue` arrives as a header
//!   task, so the first thing `response_filter` does is leave it untouched — adding a header there
//!   would corrupt the `Expect: 100-continue` handshake.

mod failure;
mod forward;
mod hooks;
mod record;

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use pingora::{http::RequestHeader, proxy::Session};

use self::failure::{write_respond, write_synthetic};
use crate::{
    context::HttpContext, error::HttpError, plugin::ResponseData, state::HttpStateHandle,
    trusted::Cidr,
};

/// Largest `x-request-id` this data plane echoes back as a trace id.
///
/// A longer value is ignored rather than truncated: half of a caller's id is worse than a
/// generated one, because it would silently stop matching the caller's own logs.
const MAX_TRACE_ID_BYTES: usize = 128;
/// The HTTP data plane.
///
/// One instance serves every request of one Pingora service: it holds the publication handle the
/// reload pipeline writes and nothing per request. Per-request state lives in [`HttpContext`], so two
/// concurrent requests cannot observe each other.
pub struct HttpApp {
    state: Arc<HttpStateHandle>,
    /// The proxies this deployment trusts to describe the caller. A peer inside the set may speak
    /// for a client through `X-Forwarded-For`; everyone else is the client.
    trusted: Arc<[Cidr]>,
    /// Backs the generated trace id. Shared by every worker, so ids are unique per process.
    sequence: AtomicU64,
}

impl HttpApp {
    /// Builds the data plane over the one publication handle it reads configuration from.
    #[must_use]
    pub fn new(state: Arc<HttpStateHandle>, trusted: Arc<[Cidr]>) -> Self {
        Self {
            state,
            trusted,
            sequence: AtomicU64::new(0),
        }
    }

    /// Returns the proxies this deployment trusts, for the client-address derivation.
    pub(crate) fn trusted_proxies(&self) -> &Arc<[Cidr]> {
        &self.trusted
    }

    /// Returns the token-budget table every request and every reload share.
    pub(crate) fn budget_store(&self) -> Arc<dyn crate::quota::BudgetStore> {
        self.state.budgets()
    }

    /// Renders `error` and tells Pingora the response is already written.
    ///
    /// A failed write means the client is gone; the request's log line already carries the outcome,
    /// so it is recorded at `debug!` rather than replacing the failure that caused it.
    async fn reject(
        &self,
        session: &mut Session,
        ctx: &mut HttpContext,
        error: HttpError,
    ) -> pingora::Result<bool> {
        ctx.fail(error);
        if let Some(respond) = ctx.error().and_then(HttpError::respond) {
            let trace_id = ctx.trace_id();
            if let Err(error) = write_respond(session, &respond, trace_id).await {
                tracing::debug!(error = %error, "the error response could not be written");
            }
        }
        Ok(true)
    }

    /// Writes the response a plugin synthesised, and tells Pingora the request is answered.
    ///
    /// This is the second caller of the one writer that answers without forwarding — the first is
    /// [`HttpApp::reject`] — and it exists so a plugin never reaches a session: the plugin produced a
    /// [`ResponseData`], the data plane cleaned it and wrote it. A failed write means the client is
    /// gone; the request's log line still carries the outcome, so it is recorded at `debug!` rather
    /// than replacing the answer that caused it.
    async fn respond(
        &self,
        session: &mut Session,
        ctx: &mut HttpContext,
        data: ResponseData,
    ) -> pingora::Result<bool> {
        let trace_id = ctx.trace_id();
        if let Err(error) = write_synthetic(session, data, trace_id).await {
            tracing::debug!(error = %error, "the synthesised response could not be written");
        }
        Ok(true)
    }

    /// Returns the correlation id for one request.
    ///
    /// The caller's `x-request-id` wins so its own trace survives the hop; otherwise this process's
    /// request number is used, which is unique per process and cheap. Neither is a distributed
    /// trace id — `nico-core` has none yet — so the field is documented as a correlation string.
    ///
    /// A caller-supplied id is used only when it is short and printable: the value lands in every
    /// log line for the request, so a control character in it would let a caller forge log records,
    /// and an unprintable one would make the line unreadable. Anything else is replaced by the
    /// generated id rather than escaped, because an escaped id no longer matches the caller's own
    /// logs — which is the only reason to keep it.
    fn trace_id(&self, header: &RequestHeader) -> String {
        if let Some(request_id) = header
            .headers
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .filter(|value| is_echoable_trace_id(value))
        {
            return request_id.to_owned();
        }
        format!("{:x}", self.sequence.fetch_add(1, Ordering::Relaxed))
    }
}

/// Reports whether a caller's correlation id may be echoed into logs and error bodies.
///
/// Printable ASCII only: the value is attacker-controlled, and the log line it is written into is
/// not a place to render control characters or non-ASCII lookalikes.
fn is_echoable_trace_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TRACE_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || byte == b' ')
}

#[cfg(test)]
mod tests;
