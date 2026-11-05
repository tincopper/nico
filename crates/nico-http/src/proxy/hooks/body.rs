//! Both body directions: meter, meter again, and hand the bytes to the chains.
//!
//! The request body has two modes and the response body has exactly one:
//!
//! - **Streamed request** (the default): each chunk goes through the global chain and then the route
//!   chain, and the bytes a plugin leaves are what the upstream receives.
//! - **Buffered request** (`buffers_body` on any plugin of the route): every chunk is held back and
//!   accumulated, the streams ends, and both chains are called **once** with the whole body — which is
//!   the only way a plugin can rewrite a body whose length it does not know.
//! - **Streamed response**, always: an event stream may never end, so no document can ask for the whole
//!   body. A plugin may still replace or drop a chunk, which is why a route whose chain rewrites bytes
//!   has its upstream `content-length` removed in the response-header hook.
//!
//! Holding a chunk back is `*body = Some(Bytes::new())` with `end_of_stream == false`: the framework
//! skips an empty chunk that is not the end (`proxy_h1.rs`), so nothing reaches the upstream until the
//! final call sets the real bytes with the end flag. `*body = None` would mean "the body ended here",
//! which is the opposite of holding back.
//!
//! The counters are also the only place the whole-request deadline can stop a *downstream* body:
//! Pingora bounds neither the time nor the rate at which a client sends one, so without this check a
//! client trickling bytes keeps the request (and the upstream connection already opened for it) alive
//! for as long as its own patience lasts.

use std::time::Instant;

use bytes::Bytes;
use pingora::proxy::Session;

use super::super::{HttpApp, failure::abort};
use crate::{context::HttpContext, error::HttpError};

/// Counts the request body, enforces its budget, and runs the body stages.
pub(super) async fn request_body_filter(
    _app: &HttpApp,
    session: &mut Session,
    body: &mut Option<Bytes>,
    end_of_stream: bool,
    ctx: &mut HttpContext,
) -> pingora::Result<()> {
    let buffered = ctx.buffers_body();
    let Some(chunk) = body.as_ref() else {
        // No bytes in this call. In the buffered mode the stream may end here with the body it
        // accumulated, and the plugins still have to see it once.
        if buffered && end_of_stream {
            return forward_whole(session, ctx, body).await;
        }
        return Ok(());
    };

    ctx.count_body(chunk.len());
    if let Some(limit) = ctx.exceeded_body_limit() {
        // A chunked upload over the bound is answered by `fail_to_proxy` before anything was
        // committed. The verdict and the limit it was measured against come from one query.
        return Err(abort(ctx, HttpError::BodyTooLarge { limit }));
    }
    if ctx.deadline_failure(Instant::now()).is_some() {
        // The body is still arriving, so the caller is the slow party and the peer is not at fault.
        // `408` is the one status that says so: a `504` here would send an operator to inspect a
        // healthy upstream. This hook owns that mapping; the context only says the clock ran out.
        return Err(abort(ctx, HttpError::RequestTimeout));
    }

    if buffered {
        ctx.push_body(chunk);
        if end_of_stream {
            return forward_whole(session, ctx, body).await;
        }
        // Hold the chunk back. `end_of_stream` is false here, so the framework skips an empty chunk
        // instead of treating it as the terminating one.
        *body = Some(Bytes::new());
        return Ok(());
    }

    let mut current = chunk.to_vec();
    current = run_request_body_stages(session, ctx, current, end_of_stream).await?;
    *body = Some(Bytes::from(current));
    Ok(())
}

/// Sends the body a buffered route accumulated, after letting the plugins see it whole.
///
/// The final call rewrites the whole body once, so the bytes set here are the only ones the upstream
/// receives — including the empty body a plugin may produce, which is a legal last chunk.
async fn forward_whole(
    session: &mut Session,
    ctx: &mut HttpContext,
    body: &mut Option<Bytes>,
) -> pingora::Result<()> {
    let whole = ctx.take_body();
    let forwarded = run_request_body_stages(session, ctx, whole, true).await?;
    *body = Some(Bytes::from(forwarded));
    Ok(())
}

/// Runs the global then the route request-body stages over one chunk, in that scope order.
async fn run_request_body_stages(
    session: &mut Session,
    ctx: &mut HttpContext,
    chunk: Vec<u8>,
    complete: bool,
) -> pingora::Result<Vec<u8>> {
    let mut current = chunk;
    {
        let Some(global) = ctx.global().cloned() else {
            return Ok(current);
        };
        let stepped = global
            .run_request_body(ctx, session.req_header_mut(), current, complete)
            .await;
        current = match stepped {
            Ok(body) => body,
            Err(error) => return Err(abort(ctx, error)),
        };
    }
    let Some(chain) = ctx.route_chain().cloned() else {
        return Ok(current);
    };
    match chain
        .run_request_body(ctx, session.req_header_mut(), current, complete)
        .await
    {
        Ok(body) => Ok(body),
        Err(error) => Err(abort(ctx, error)),
    }
}

/// Runs the response-body stages between two checks of the request's remaining budget.
///
/// This stage is synchronous because the framework hook is: an `async` stage cannot be enclosed by
/// `timeout_at` here, so the only way to bound it is to read the same clock before and after. Both
/// edges go through [`budget_guard`], so adding a third exit to the stage logic cannot skip the
/// after-check. A plugin that blocks past the deadline is recorded as cancelled even when it ran on
/// the final chunk — otherwise no later chunk would ever notice.
pub(super) fn upstream_response_body_filter(
    _app: &HttpApp,
    session: &mut Session,
    body: &mut Option<Bytes>,
    end_of_stream: bool,
    ctx: &mut HttpContext,
) -> pingora::Result<Option<std::time::Duration>> {
    budget_guard(ctx)?;
    response_body_stages(session, body, end_of_stream, ctx)?;
    budget_guard(ctx)?;
    Ok(None)
}

/// Aborts the stream when the request has spent its whole budget.
///
/// The status line is already downstream by the time a response-body stage runs, so a deadline
/// failure cannot become a `504`: the stream ends and the request is recorded as cancelled.
fn budget_guard(ctx: &mut HttpContext) -> pingora::Result<()> {
    if let Some(error) = ctx.deadline_failure(Instant::now()) {
        ctx.mark_cancelled();
        return Err(abort(ctx, error));
    }
    Ok(())
}

/// Hands one downstream chunk through the two scopes, in route-then-global order.
fn response_body_stages(
    session: &mut Session,
    body: &mut Option<Bytes>,
    end_of_stream: bool,
    ctx: &mut HttpContext,
) -> pingora::Result<()> {
    let Some(chunk) = body.take() else {
        // No bytes for this task: there is no body stage to run, and `end_of_stream` alone is not a
        // chunk a plugin could replace.
        return Ok(());
    };
    if ctx.route_chain().is_none() && ctx.global().is_none() {
        *body = Some(chunk);
        return Ok(());
    }

    // The head has been written, so the stages read it as sent: `response_written` is the exact header
    // the client received, and a write attempt is a phase violation rather than an edit nobody sees.
    // A head that is missing (the write is still queued) does not skip the stage — a rewrite must never
    // be silently dropped — it only makes a response read answer `PhaseViolation`.
    let head = session.response_written();
    let mut current = chunk;
    for chain in [ctx.route_chain().cloned(), ctx.global().cloned()] {
        let Some(chain) = chain else {
            continue;
        };
        match chain.run_response_body(ctx, head, current, end_of_stream) {
            Ok(Some(next)) => current = next,
            Ok(None) => {
                // A plugin emptied the chunk: send nothing for it, and do not offer the bytes to the
                // next scope — there are none. Only a route whose response cannot be length-framed
                // reaches this, because otherwise the header would no longer describe the body.
                *body = None;
                return Ok(());
            }
            Err(error) => return Err(abort(ctx, error)),
        }
    }
    *body = Some(current);
    Ok(())
}
