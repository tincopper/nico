//! The Pingora hook contract: one snapshot read, one order, one response per failure.
//!
//! The order is the contract, not an implementation detail, and every row below is a decision that a
//! Pingora upgrade can invalidate:
//!
//! | Hook | Plugin stages it drives | Why there |
//! |---|---|---|
//! | `new_ctx` | — | request-local state, never global |
//! | `request_filter` | **global `on_request`**, then routing, then **route `on_request`** | the global stage runs before a route exists, so it sees no route id and no identity |
//! | `upstream_peer` | — | the target becomes a connectable peer, bounded by the remaining budget |
//! | `upstream_request_filter` | — | `Host`, `X-Forwarded-*`, and chunked framing for a buffered body |
//! | `request_body_filter` | `on_request_body`, global then route | per chunk, or once for a buffered route |
//! | `upstream_response_filter` | `on_response`, **route then global** | the head is not downstream yet, so the status and headers can still change |
//! | `upstream_response_body_filter` | `on_response_body`, **route then global** | per chunk, synchronous: the response stream is framed already |
//! | `response_filter` | — | informational responses are left untouched |
//! | `error_while_proxy` | — | classification only: it is synchronous and writes nothing |
//! | `fail_to_proxy` | — | the one place a failure becomes a response |
//! | `logging` | `on_logging`, **route then global** | after the response, so it cannot change it |
//!
//! Response and logging run in the reverse scope order — route first, global last — because a scope
//! that published later has to be the last to withdraw, the same way nested middleware unwinds.
//!
//! Three facts about the framework shape the whole module:
//!
//! - **A hook that consumes a request-body chunk never gives it back.** The bodies are streamed and not
//!   replayed, so a plugin receives the chunk through the context and returns the bytes to send; it
//!   never reads the session.
//! - **The response-body hook is synchronous.** A plugin stage there cannot await, which is why
//!   [`Plugin::on_response_body`](crate::Plugin::on_response_body) is the one synchronous stage.
//! - **Informational responses reach the response hooks.** A `100 Continue` arrives as a header task,
//!   so anything a plugin added there would corrupt the `Expect: 100-continue` handshake.
//!
//! The bodies of the hooks live in the submodules, one per direction: `request` decides whether the
//! request is served at all, `body` moves the bytes, `response` closes the request out, and `llm`
//! translates the two bodies of a provider route. This file is the assembly — the `ProxyHttp`
//! implementation itself must be one block, so each method is a delegation and the order above is the
//! only thing it states.
//!
//! A route that targets a provider changes two rows of the table: the body stages and the response
//! stages do not run, because the bytes belong to a client protocol the policy cannot see. The request
//! stages still run (authentication and rate limiting are header decisions), and so does the logging
//! stage. Publication rejects a document that asks for the other combination; the hooks skip the
//! stages anyway, so the guarantee does not depend on the compiler having refused it.

mod body;
pub(super) mod llm;
mod request;
mod response;

use pingora::{
    Error,
    proxy::{ProxyHttp, Session},
    upstreams::peer::HttpPeer,
};

use super::HttpApp;
use crate::context::HttpContext;

#[async_trait::async_trait]
impl ProxyHttp for HttpApp {
    type CTX = HttpContext;

    fn new_ctx(&self) -> Self::CTX {
        HttpContext::default()
    }

    async fn request_filter(
        &self,
        session: &mut pingora::proxy::Session,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<bool> {
        request::request_filter(self, session, ctx).await
    }

    async fn upstream_peer(
        &self,
        session: &mut pingora::proxy::Session,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<Box<pingora::upstreams::peer::HttpPeer>> {
        request::upstream_peer(self, session, ctx).await
    }

    async fn upstream_request_filter(
        &self,
        session: &mut pingora::proxy::Session,
        upstream_request: &mut pingora::http::RequestHeader,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<()> {
        request::upstream_request_filter(self, session, upstream_request, ctx).await
    }

    async fn request_body_filter(
        &self,
        session: &mut pingora::proxy::Session,
        body: &mut Option<bytes::Bytes>,
        end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<()> {
        body::request_body_filter(self, session, body, end_of_stream, ctx).await
    }

    async fn upstream_response_filter(
        &self,
        session: &mut pingora::proxy::Session,
        upstream_response: &mut pingora::http::ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<()> {
        response::upstream_response_filter(self, session, upstream_response, ctx).await
    }

    fn upstream_response_body_filter(
        &self,
        session: &mut pingora::proxy::Session,
        body: &mut Option<bytes::Bytes>,
        end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<Option<std::time::Duration>> {
        body::upstream_response_body_filter(self, session, body, end_of_stream, ctx)
    }

    async fn response_filter(
        &self,
        session: &mut pingora::proxy::Session,
        upstream_response: &mut pingora::http::ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<()> {
        response::response_filter(self, session, upstream_response, ctx).await
    }

    fn fail_to_connect(
        &self,
        session: &mut Session,
        peer: &HttpPeer,
        ctx: &mut Self::CTX,
        error: Box<Error>,
    ) -> Box<Error> {
        response::fail_to_connect(self, session, peer, error, ctx)
    }

    fn error_while_proxy(
        &self,
        peer: &pingora::upstreams::peer::HttpPeer,
        session: &mut pingora::proxy::Session,
        error: Box<pingora::Error>,
        ctx: &mut Self::CTX,
        client_reused: bool,
    ) -> Box<pingora::Error> {
        response::error_while_proxy(self, peer, session, error, ctx, client_reused)
    }

    async fn fail_to_proxy(
        &self,
        session: &mut pingora::proxy::Session,
        error: &pingora::Error,
        ctx: &mut Self::CTX,
    ) -> pingora::proxy::FailToProxy {
        response::fail_to_proxy(self, session, error, ctx).await
    }

    async fn logging(
        &self,
        session: &mut pingora::proxy::Session,
        error: Option<&pingora::Error>,
        ctx: &mut Self::CTX,
    ) {
        response::logging(self, session, error, ctx).await;
    }
}
