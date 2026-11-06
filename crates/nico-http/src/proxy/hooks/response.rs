//! Closing the request out: the response head, the failure writer, and the one log record.
//!
//! Three ordering facts are the contract here:
//!
//! - **`on_response` runs in `upstream_response_filter`**, before anything is written downstream, so a
//!   plugin can still change the status and the headers. It runs **route then global**: the scope that
//!   published later withdraws last.
//! - **Informational responses are untouched.** A `100 Continue` reaches the same hooks and may be
//!   followed by the real response, so a header added there would corrupt the handshake; the guard sits
//!   in the hook that runs first.
//! - **`fail_to_proxy` is the only writer of a failure.** A hook that fails records an [`HttpError`] and
//!   returns an error; the failure becomes a response exactly once, in one function, with one header set.
//!
//! `on_logging` runs in `logging`, after the response is out. It cannot change anything, cannot fail the
//! request, and its plugins run route then global like the response stages.

use std::time::Instant;

use pingora::{
    Error,
    http::{Method, ResponseHeader},
    proxy::{FailToProxy, Session},
    upstreams::peer::HttpPeer,
};

use super::super::{
    HttpApp,
    failure::{abort, classify, is_downstream_gone, write_respond},
    hooks::llm,
    record,
};
use crate::{context::HttpContext, error::HttpError};

/// Runs the response stages, in the reverse scope order, before the head is written.
///
/// A route whose chain rewrites response bytes cannot keep the upstream's `content-length`: the plugin
/// will change those bytes chunk by chunk. Removing the header here is what makes the framework frame
/// the response as chunked instead — it only adds a transfer coding when neither header is present, so
/// the removal has to happen before that point.
///
/// An LLM route takes the other branch: its response head is the client protocol's answer, produced by
/// the transform this hook installs, and no plugin response stage runs — a policy must not rewrite the
/// bytes of a protocol it cannot see.
pub(super) async fn upstream_response_filter(
    _app: &HttpApp,
    session: &mut Session,
    response: &mut ResponseHeader,
    ctx: &mut HttpContext,
) -> pingora::Result<()> {
    // CONTEXT: a `100 Continue` reaches this hook as an informational response, which may be followed
    // by the real one. Anything a plugin added here would be appended to that interim response.
    if response.status.is_informational() {
        return Ok(());
    }

    if ctx.llm().is_some() {
        let request = session.req_header();
        if let Err(error) = llm::install_transform(ctx, request, response) {
            return Err(abort(ctx, error));
        }
        if ctx.expired(Instant::now()) {
            let error = ctx.deadline_error();
            return Err(abort(ctx, error));
        }
        return Ok(());
    }

    if ctx.rewrites_response()
        && may_carry_body(session.req_header().method == Method::HEAD, response)
    {
        response.remove_header("content-length");
    }

    for chain in [ctx.route_chain().cloned(), ctx.global().cloned()] {
        let Some(chain) = chain else {
            continue;
        };
        // Route first, then global: the scope that published later withdraws last, the same order the
        // logging stages run in.
        if let Err(error) = chain.run_response(ctx, response).await {
            return Err(abort(ctx, error));
        }
    }

    if ctx.expired(Instant::now()) {
        let error = ctx.deadline_error();
        return Err(abort(ctx, error));
    }
    Ok(())
}

/// Returns whether a response can still carry a body at all.
///
/// Only such a response may lose its `content-length`, and only such a response gives a transform
/// bytes to translate. A response that carries no body — `HEAD`, a `204`/`304`, or an upstream that
/// declared a length of zero — keeps the header: there is nothing to rewrite, no chunk will arrive, and
/// a head with neither a length nor a transfer coding is a response the client reads until the
/// connection closes.
pub(super) fn may_carry_body(head_request: bool, response: &ResponseHeader) -> bool {
    if head_request {
        return false;
    }
    if matches!(response.status.as_u16(), 204 | 304) {
        return false;
    }
    response
        .headers
        .get("content-length")
        .is_none_or(|value| value.as_bytes() != b"0")
}

/// Guards the informational response and re-checks the deadline.
///
/// The response plugins ran in `upstream_response_filter`, which is earlier and still before the wire:
/// running them a second time here would apply every rewrite twice.
pub(super) async fn response_filter(
    _app: &HttpApp,
    _session: &mut Session,
    response: &mut ResponseHeader,
    ctx: &mut HttpContext,
) -> pingora::Result<()> {
    if response.status.is_informational() {
        return Ok(());
    }
    if ctx.expired(Instant::now()) {
        let error = ctx.deadline_error();
        return Err(abort(ctx, error));
    }
    Ok(())
}

/// Classifies a failure Pingora reported, so a disconnect is never counted as a server fault.
pub(super) fn error_while_proxy(
    _app: &HttpApp,
    _peer: &HttpPeer,
    _session: &mut Session,
    error: Box<Error>,
    ctx: &mut HttpContext,
    _client_reused: bool,
) -> Box<Error> {
    if is_downstream_gone(&error) {
        ctx.mark_cancelled();
    }
    // One attempt per request (`ServerConf::max_retries == 1`), so the error is not retryable: this
    // build never retries, and Pingora would otherwise log a phantom retry.
    let mut error = error;
    error.set_retry(false);
    error
}

/// Writes the one response a proxy failure is allowed to produce.
///
/// Pingora calls this exactly once per request that failed after the response stages, whatever the
/// stage, and its default would write a framework error page. Installing our own keeps the body shape
/// contract and leaves exactly one writer of failure responses in the proxy path.
pub(super) async fn fail_to_proxy(
    _app: &HttpApp,
    session: &mut Session,
    error: &Error,
    ctx: &mut HttpContext,
) -> FailToProxy {
    if ctx.error().is_none() {
        match classify(ctx, error) {
            Some(failure) => ctx.fail(failure),
            None => {
                // The client is gone: there is nobody left to answer.
                ctx.mark_cancelled();
                return FailToProxy {
                    error_code: 0,
                    can_reuse_downstream: false,
                };
            }
        }
    }

    if session.response_written().is_some() {
        // The status line is already downstream, so this failure cannot become a response: the client
        // sees a stream that ended early, not a 5xx. Recording it as a cancellation is what keeps an
        // already-delivered response out of the operator's error stream; the cause stays in the log
        // through `error = %failure`.
        ctx.mark_cancelled();
        return FailToProxy {
            error_code: 0,
            can_reuse_downstream: false,
        };
    }
    if ctx.is_cancelled() {
        return FailToProxy {
            error_code: 0,
            can_reuse_downstream: false,
        };
    }

    let Some(respond) = ctx.error().and_then(HttpError::respond) else {
        // `ResponseCommitted`: our own vocabulary says the failure cannot be rendered.
        ctx.mark_cancelled();
        return FailToProxy {
            error_code: 0,
            can_reuse_downstream: false,
        };
    };

    let status = respond.status().as_u16();
    if let Err(error) = write_respond(session, &respond, ctx.trace_id()).await {
        tracing::debug!(error = %error, "the error response could not be written");
    }
    FailToProxy {
        error_code: status,
        // A request that reached an upstream may have consumed body bytes; reusing the connection
        // would risk a desynchronised stream on the next request.
        can_reuse_downstream: false,
    }
}

/// Runs the logging stages, then emits the request's one structured record.
///
/// The stages come first: they exist to report what only a plugin knows, and the record is the data
/// plane's own account of the request. Neither can affect the response, which is already out. The
/// response a plugin sees here is the one the client received — `response_written` — so a request that
/// never got one still reaches the stage, with the response facts answering a phase violation.
pub(super) async fn logging(
    _app: &HttpApp,
    session: &mut Session,
    error: Option<&Error>,
    ctx: &mut HttpContext,
) {
    let head = session.response_written();
    for chain in [ctx.route_chain().cloned(), ctx.global().cloned()] {
        let Some(chain) = chain else {
            continue;
        };
        chain.run_logging(ctx, head).await;
    }
    record::emit(session, error, ctx);
}
