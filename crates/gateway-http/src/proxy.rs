//! The HTTP data plane: one type, one hook contract, one response per failure.
//!
//! [`HttpApp`] is what Pingora drives. It holds exactly two things — the publication handle the
//! reload pipeline writes and the filter chain — and no per-request state, which lives in
//! [`HttpContext`] instead so two concurrent requests cannot observe each other.
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
//! - **A filter that reads the request body never gives it back.** Phase-0 evidence: the body
//!   arrives in chunks with unstable boundaries and is not replayed, so a route decision made from
//!   the body would leave the upstream waiting forever. Every decision here uses headers only.
//! - **`error_while_proxy` is synchronous.** It cannot write a response, so it is used only to
//!   classify a failure; the response is written by `fail_to_proxy`, which is asynchronous and runs
//!   exactly once for the final failure of a request.
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

use gateway_core::SnapshotHandle;
use pingora::{http::RequestHeader, proxy::Session};

use self::failure::write_respond;
use crate::{context::HttpContext, error::HttpError, filter::FilterChain};

/// Largest `x-request-id` this data plane echoes back as a trace id.
///
/// A longer value is ignored rather than truncated: half of a caller's id is worse than a
/// generated one, because it would silently stop matching the caller's own logs.
const MAX_TRACE_ID_BYTES: usize = 128;
/// The HTTP data plane.
///
/// One instance serves every request of one Pingora service: it holds the publication handle the
/// reload pipeline writes and the filter chain, and nothing per request. Per-request state lives in
/// [`HttpContext`], so two concurrent requests cannot observe each other.
pub struct HttpApp {
    snapshot: Arc<SnapshotHandle>,
    filters: FilterChain,
    /// Backs the generated trace id. Shared by every worker, so ids are unique per process.
    sequence: AtomicU64,
}

impl HttpApp {
    /// Builds the data plane over one publication handle and one filter chain.
    #[must_use]
    pub fn new(snapshot: Arc<SnapshotHandle>, filters: FilterChain) -> Self {
        Self {
            snapshot,
            filters,
            sequence: AtomicU64::new(0),
        }
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
            if let Err(error) = write_respond(session, respond, trace_id).await {
                tracing::debug!(error = %error, "the error response could not be written");
            }
        }
        Ok(true)
    }

    /// Returns the correlation id for one request.
    ///
    /// The caller's `x-request-id` wins so its own trace survives the hop; otherwise this process's
    /// request number is used, which is unique per process and cheap. Neither is a distributed
    /// trace id — `gateway-core` has none yet — so the field is documented as a correlation string.
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
