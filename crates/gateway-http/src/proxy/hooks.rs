//! The Pingora hook contract: one snapshot read, one order, one response per failure.
//!
//! The order is the contract, not an implementation detail:
//!
//! | Hook | What it does | Why there |
//! |---|---|---|
//! | `new_ctx` | empty request context | request-local state, never global |
//! | `request_filter` | snapshot, route, `Content-Length`, filter chain | before the body, which is never read |
//! | `upstream_peer` | target becomes a peer | route and upstream are known by now |
//! | `upstream_request_filter` | `Host` rewrite, `X-Forwarded-*`, hop-by-hop removal | the upstream sees only legal headers |
//! | `request_body_filter` | counts bytes, enforces the body bound and the deadline | streaming: the body is metered, never buffered |
//! | `response_filter` | response filters, deadline | the last point that can still change the status |
//! | `upstream_response_body_filter` | deadline | guarantees the body is never rewritten |
//! | `fail_to_proxy` | the one place a proxy failure becomes a response | Pingora calls it once per failed request |
//! | `logging` | one structured request record | so no filter logs and the count stays one per request |
//!
//! Two hooks are deliberately absent. `upstream_response_filter` would have an empty body: nothing
//! this milestone does modifies an upstream response, and the informational-response trap it exists
//! to guard is handled where a header is actually touched, in `response_filter`. `error_while_proxy`
//! is synchronous, so it classifies a failure and writes nothing; `fail_to_proxy` is the one writer.

use std::{io, time::Instant};

use bytes::Bytes;
use pingora::{
    Error,
    http::{RequestHeader, ResponseHeader},
    proxy::{FailToProxy, ProxyHttp, Session},
    upstreams::peer::HttpPeer,
};

use super::{
    HttpApp,
    failure::{abort, classify, is_downstream_gone, write_respond},
    forward::{HOP_BY_HOP, connection_tokens, destination, forwarded_for, set_header},
    record,
};
use crate::{context::HttpContext, error::HttpError, limits, route, upstream};

#[async_trait::async_trait]
impl ProxyHttp for HttpApp {
    type CTX = HttpContext;

    fn new_ctx(&self) -> Self::CTX {
        HttpContext::default()
    }

    /// Routes the request, or answers it without ever contacting an upstream.
    ///
    /// Every rejection funnels through one writer, so the only difference between "no snapshot",
    /// "no route", "body too large" and "a filter said no" is the [`HttpError`] value.
    async fn request_filter(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<bool> {
        let now = Instant::now();
        let trace_id = self.trace_id(session.req_header());
        ctx.set_trace_id(&trace_id);

        let Some(snapshot) = self.snapshot.current() else {
            return self.reject(session, ctx, HttpError::NotReady).await;
        };

        let Some(route) = route::match_route(session.req_header(), snapshot.document().routes())
        else {
            return self.reject(session, ctx, HttpError::RouteNotFound).await;
        };

        // The immutable borrow of the request ends here: everything routing needed has been copied
        // into the context, and the filter chain below needs the header mutably.
        let declared = limits::declared_length(session.req_header());
        ctx.begin(&snapshot, route, session.req_header(), now);

        if let (Some(limit), Some(declared)) = (ctx.body_limit(), declared) {
            if limits::over_limit(limit, declared) {
                return self
                    .reject(session, ctx, HttpError::BodyTooLarge { limit })
                    .await;
            }
        }

        if let Err(error) = self
            .filters
            .run_request(ctx, session.req_header_mut())
            .await
        {
            return self.reject(session, ctx, error).await;
        }

        Ok(false)
    }

    /// Turns the matched target into a peer, bounded by the request's remaining budget.
    ///
    /// The peer is decided once and recorded in the context: the `Host` rewrite that follows reads
    /// the same destination instead of resolving the target a second time, so a filter-chosen peer
    /// and the header sent to it cannot disagree.
    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<Box<HttpPeer>> {
        let now = Instant::now();
        // An expired budget spends no resolver query and opens no socket: the answer is already
        // known, and it is the request's own clock that decided it.
        let budget = ctx.remaining(now).unwrap_or_default();
        if budget.is_zero() {
            let error = ctx.deadline_error();
            return Err(abort(ctx, error));
        }

        let chosen = destination(ctx).map_err(|error| abort(ctx, error))?;
        let peer = upstream::peer(&chosen, budget).await;
        ctx.set_destination(chosen);
        peer.map_err(|error| abort(ctx, error))
    }

    /// Rewrites the request headers for the upstream and removes what must not travel further.
    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<()> {
        // Both reads borrow the downstream session immutably; the header below is a different
        // object, so the borrows do not overlap.
        let client_host = session.req_header().headers.get("host").cloned();
        let client_addr = session
            .client_addr()
            .and_then(pingora::protocols::l4::socket::SocketAddr::as_inet)
            .map(|address| address.ip());

        // The `Connection` header nominates a second, open-ended set of headers that belong to
        // this connection only; read it before the fixed set removes it.
        let nominated = connection_tokens(request).map_err(|error| abort(ctx, error))?;
        for name in HOP_BY_HOP {
            request.remove_header(name);
        }
        for name in &nominated {
            request.remove_header(name.as_str());
        }

        // `upstream_peer` recorded the destination it connected to, so this hook only reads it.
        let Some(destination) = ctx.destination() else {
            return Err(abort(
                ctx,
                HttpError::Internal {
                    source: Box::new(io::Error::other(
                        "no destination was recorded before the request was forwarded",
                    )),
                },
            ));
        };
        let host = upstream::host_header(destination);
        if let Some(host) = client_host.as_ref().and_then(|value| value.to_str().ok()) {
            set_header(request, "x-forwarded-host", host, ctx)?;
        }
        if let Some(forwarded_for) = forwarded_for(request, client_addr) {
            set_header(request, "x-forwarded-for", &forwarded_for, ctx)?;
        }
        // Only the outermost hop states the scheme: appending a second value produces a list that
        // no server parses, and this build terminates no TLS, so it can only ever say `http`.
        if request.headers.get("x-forwarded-proto").is_none() {
            set_header(request, "x-forwarded-proto", "http", ctx)?;
        }
        set_header(request, "host", &host, ctx)?;

        Ok(())
    }

    /// Counts the request body; it never buffers, rewrites or replays a chunk.
    ///
    /// The counter is also the only place the whole-request deadline can stop a *downstream* body:
    /// Pingora bounds neither the time nor the rate at which a client sends one, so without this
    /// check a client trickling bytes keeps the request (and the upstream connection already opened
    /// for it) alive for as long as its own patience lasts. The peer's read timeout would eventually
    /// fire, but it cannot tell a slow caller from a slow peer — this hook can.
    async fn request_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        _end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<()> {
        let Some(chunk) = body.as_ref() else {
            return Ok(());
        };
        ctx.count_body(chunk.len());
        if ctx.body_over_limit() {
            // A chunked upload over the bound is answered by `fail_to_proxy` before anything was
            // committed; if the limit is ever missing that is a bug, not a `0`-byte limit.
            let limit = body_limit_of(ctx);
            return Err(abort(ctx, HttpError::BodyTooLarge { limit }));
        }
        if ctx.expired(Instant::now()) {
            // The body is still arriving, so the caller is the slow party and the peer is not at
            // fault. `408` is the one status that says so: a `504` here would send an operator to
            // inspect a healthy upstream.
            let error = HttpError::RequestTimeout;
            return Err(abort(ctx, error));
        }
        Ok(())
    }

    /// Runs the response filters and re-checks the deadline; it never rewrites the body.
    async fn response_filter(
        &self,
        _session: &mut Session,
        response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<()> {
        // PHASE-0: a `100 Continue` reaches this hook as an informational response, which may be
        // followed by the real one. Anything a filter adds here would be appended to that interim
        // response and break the handshake.
        if response.status.is_informational() {
            return Ok(());
        }

        if let Err(error) = self.filters.run_response(ctx, response).await {
            return Err(abort(ctx, error));
        }

        if ctx.expired(Instant::now()) {
            let error = ctx.deadline_error();
            return Err(abort(ctx, error));
        }

        Ok(())
    }

    /// Re-checks the deadline while streaming, and leaves the chunk alone.
    fn upstream_response_body_filter(
        &self,
        _session: &mut Session,
        _body: &mut Option<Bytes>,
        _end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<Option<std::time::Duration>> {
        if ctx.expired(Instant::now()) {
            // The status line is already downstream, so it cannot be replaced by a 504: abort the
            // stream and record the request as cancelled rather than appending a second response.
            ctx.mark_cancelled();
            let error = ctx.deadline_error();
            return Err(abort(ctx, error));
        }
        Ok(None)
    }

    /// Classifies a failure Pingora reported, so a disconnect is never counted as a server fault.
    fn error_while_proxy(
        &self,
        _peer: &HttpPeer,
        _session: &mut Session,
        error: Box<Error>,
        ctx: &mut Self::CTX,
        _client_reused: bool,
    ) -> Box<Error> {
        if is_downstream_gone(&error) {
            ctx.mark_cancelled();
        }
        // One attempt per request (`ServerConf::max_retries == 1`), so the error is not retryable:
        // this build never retries, and Pingora would otherwise log a phantom retry.
        let mut error = error;
        error.set_retry(false);
        error
    }

    /// Writes the one response a proxy failure is allowed to produce.
    ///
    /// Pingora calls this exactly once per request that failed after the filters, whatever the
    /// stage, and its default would write a framework error page. Installing our own keeps the body
    /// shape contract and leaves exactly one writer of failure responses in the proxy path.
    async fn fail_to_proxy(
        &self,
        session: &mut Session,
        error: &Error,
        ctx: &mut Self::CTX,
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
            // The status line is already downstream, so this failure cannot become a response: the
            // client sees a stream that ended early, not a 5xx. Recording it as a cancellation is
            // what keeps an already-delivered response out of the operator's error stream; the
            // cause stays in the log through `error = %failure`.
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
        if let Err(error) = write_respond(session, respond, ctx.trace_id()).await {
            tracing::debug!(error = %error, "the error response could not be written");
        }
        FailToProxy {
            error_code: status,
            // A request that reached an upstream may have consumed body bytes; reusing the
            // connection would risk a desynchronised stream on the next request.
            can_reuse_downstream: false,
        }
    }

    /// Emits the request's one structured record (what it says lives in `record`).
    async fn logging(&self, session: &mut Session, error: Option<&Error>, ctx: &mut Self::CTX) {
        record::emit(session, error, ctx);
    }
}

/// Returns the body limit `HttpContext::begin` recorded.
///
/// `begin` always records one, so a missing limit is a defect in this crate; failing loudly beats
/// rendering the rejection as a `0`-byte limit, which would send whoever reads it in the wrong
/// direction.
#[expect(
    clippy::expect_used,
    reason = "HttpContext::begin always records the body limit"
)]
fn body_limit_of(ctx: &HttpContext) -> u32 {
    ctx.body_limit().expect("begin records the body limit")
}
