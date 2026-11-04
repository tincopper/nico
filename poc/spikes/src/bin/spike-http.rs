//! Pingora 0.9.1 HTTP data-plane probe.
//!
//! Observes, per request: downstream protocol, request/response body chunking,
//! upstream connection timing, SSE write rhythm, cancel and shutdown behaviour.
//! Everything is one-shot PoC code.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use pingora::apps::HttpServerOptions;
use pingora::proxy::{ProxyHttp, ProxyServiceBuilder, Session};
use pingora::server::Server;
use pingora::server::configuration::ServerConf;
use pingora::upstreams::peer::{ALPN, HttpPeer};
use pingora::{Error, Result};
use pingora::http::ResponseHeader;
use pingora_core::protocols::Digest;
use spikes::{init_tracing, now_ms};
use tracing::info;

#[derive(Debug, Default)]
struct Ctx {
    proto: String,
    method: String,
    path: String,
    t_first_body: Option<Instant>,
    req_chunks: usize,
    req_bytes: usize,
    req_last_chunk_bytes: usize,
    t_response_head: Option<Instant>,
    resp_chunks_upstream: usize,
    resp_chunks_downstream: usize,
    resp_bytes_upstream: usize,
    resp_bytes_downstream: usize,
    upstream_build_ms: Option<u64>,
    upstream_connected_ms: Option<u64>,
    first_upstream_req_ms: Option<u64>,
    status: u16,
    start: Option<Instant>,
}

impl Ctx {
    fn elapsed_ms(&self) -> u64 {
        self.start
            .map_or(0, |s| s.elapsed().as_millis() as u64)
    }
}

struct SpikeProxy {
    upstream: String,
}

impl SpikeProxy {
    fn new(upstream: String) -> Self {
        Self { upstream }
    }
}

#[async_trait]
impl ProxyHttp for SpikeProxy {
    type CTX = Ctx;

    fn new_ctx(&self) -> Self::CTX {
        Ctx {
            start: Some(Instant::now()),
            ..Ctx::default()
        }
    }

    async fn early_request_filter(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        let head = session.req_header();
        ctx.proto = if session.is_http2() { "h2".to_string() } else { "h1".to_string() };
        ctx.method = head.method.to_string();
        ctx.path = head.uri.path().to_string();
        info!(
            t_ms = now_ms(),
            re = "early_request_filter",
            proto = ctx.proto,
            method = %ctx.method,
            path = %ctx.path,
            version = ?head.version,
            headers = ?head.headers,
            upgrade = session.is_upgrade_req(),
            "spike.req.head"
        );
        Ok(())
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        // Probe: can a filter consume the whole request body before picking the
        // upstream, and does Pingora still forward those bytes upstream?
        if ctx.path == "/peek" {
            let mut all = Vec::new();
            loop {
                match session.read_request_body().await? {
                    Some(bytes) => all.extend_from_slice(&bytes),
                    None => break,
                }
            }
            info!(
                t_ms = now_ms(),
                re = "upstream_peer.peek",
                elapsed_ms = ctx.elapsed_ms(),
                peeked_bytes = all.len(),
                peeked_sha256 = %spikes::sha256_hex(&all),
                "spike.req.peeked"
            );
        }
        let mut peer = HttpPeer::new(self.upstream.clone(), false, "mock".to_string());
        // gRPC needs h2 end-to-end; everything else exercises the default h1 pool.
        peer.options.alpn = if ctx.path == "/grpc" {
            ALPN::H2
        } else {
            ALPN::H1
        };
        ctx.upstream_build_ms = Some(ctx.elapsed_ms());
        info!(
            t_ms = now_ms(),
            re = "upstream_peer",
            elapsed_ms = ctx.elapsed_ms(),
            alpn = ?peer.options.alpn,
            upstream = %self.upstream,
            "spike.upstream.peer"
        );
        Ok(Box::new(peer))
    }

    async fn connected_to_upstream(
        &self,
        _session: &mut Session,
        reused: bool,
        peer: &HttpPeer,
        _fd: std::os::unix::io::RawFd,
        _digest: Option<&Digest>,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        ctx.upstream_connected_ms = Some(ctx.elapsed_ms());
        info!(
            t_ms = now_ms(),
            re = "connected_to_upstream",
            elapsed_ms = ctx.elapsed_ms(),
            reused,
            peer = %peer,
            first_body_chunk_seen = ctx.req_chunks > 0,
            "spike.upstream.connected"
        );
        Ok(())
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut pingora::http::RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        ctx.first_upstream_req_ms = Some(ctx.elapsed_ms());
        info!(
            t_ms = now_ms(),
            re = "upstream_request_filter",
            elapsed_ms = ctx.elapsed_ms(),
            version = ?upstream_request.version,
            headers = ?upstream_request.headers,
            "spike.upstream.request"
        );
        Ok(())
    }

    async fn request_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<bytes::Bytes>,
        end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        let len = body.as_ref().map_or(0, |b| b.len());
        if len > 0 {
            ctx.req_chunks += 1;
            ctx.req_bytes += len;
            ctx.req_last_chunk_bytes = len;
            ctx.t_first_body.get_or_insert_with(Instant::now);
        }
        info!(
            t_ms = now_ms(),
            re = "request_body_filter",
            elapsed_ms = ctx.elapsed_ms(),
            chunk = ctx.req_chunks,
            chunk_len = len,
            total = ctx.req_bytes,
            eos = end_of_stream,
            "spike.req.body"
        );
        Ok(())
    }

    async fn upstream_response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        ctx.t_response_head = Some(Instant::now());
        ctx.status = upstream_response.status.as_u16();
        info!(
            t_ms = now_ms(),
            re = "upstream_response_filter",
            elapsed_ms = ctx.elapsed_ms(),
            status = ctx.status,
            version = ?upstream_response.version,
            headers = ?upstream_response.headers,
            "spike.upstream.response"
        );
        upstream_response.insert_header("x-spike-proto", ctx.proto.clone())?;
        upstream_response.insert_header("x-spike-req-chunks", ctx.req_chunks.to_string())?;
        upstream_response.insert_header("x-spike-req-bytes", ctx.req_bytes.to_string())?;
        Ok(())
    }

    fn upstream_response_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<bytes::Bytes>,
        end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<Option<Duration>> {
        let len = body.as_ref().map_or(0, |b| b.len());
        if len > 0 {
            ctx.resp_chunks_upstream += 1;
            ctx.resp_bytes_upstream += len;
        }
        info!(
            t_ms = now_ms(),
            re = "upstream_response_body_filter",
            since_headers_ms = ctx
                .t_response_head
                .map_or(0, |t| t.elapsed().as_millis() as u64),
            chunk = ctx.resp_chunks_upstream,
            chunk_len = len,
            total = ctx.resp_bytes_upstream,
            eos = end_of_stream,
            "spike.upstream.body"
        );
        Ok(None)
    }

    fn response_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<bytes::Bytes>,
        end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<Option<Duration>> {
        let len = body.as_ref().map_or(0, |b| b.len());
        if len > 0 {
            ctx.resp_chunks_downstream += 1;
            ctx.resp_bytes_downstream += len;
        }
        info!(
            t_ms = now_ms(),
            re = "response_body_filter",
            chunk = ctx.resp_chunks_downstream,
            chunk_len = len,
            total = ctx.resp_bytes_downstream,
            eos = end_of_stream,
            "spike.resp.body"
        );
        Ok(None)
    }

    fn upstream_response_trailer_filter(
        &self,
        _session: &mut Session,
        upstream_trailers: &mut http::HeaderMap,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        info!(
            t_ms = now_ms(),
            re = "upstream_response_trailer_filter",
            trailers = ?upstream_trailers,
            "spike.upstream.trailers"
        );
        let _ = ctx;
        Ok(())
    }

    async fn logging(
        &self,
        session: &mut Session,
        error: Option<&Error>,
        ctx: &mut Self::CTX,
    ) {
        info!(
            t_ms = now_ms(),
            re = "logging",
            elapsed_ms = ctx.elapsed_ms(),
            method = %ctx.method,
            path = %ctx.path,
            proto = %ctx.proto,
            worker = ?std::thread::current().id(),
            status = ctx.status,
            req_chunks = ctx.req_chunks,
            req_bytes = ctx.req_bytes,
            req_last_chunk_bytes = ctx.req_last_chunk_bytes,
            resp_chunks_upstream = ctx.resp_chunks_upstream,
            resp_chunks_downstream = ctx.resp_chunks_downstream,
            resp_bytes_upstream = ctx.resp_bytes_upstream,
            resp_bytes_downstream = ctx.resp_bytes_downstream,
            upstream_peer_ms = ctx.upstream_build_ms,
            upstream_connected_ms = ctx.upstream_connected_ms,
            upstream_request_ms = ctx.first_upstream_req_ms,
            body_bytes_read = session.body_bytes_read(),
            body_bytes_sent = session.body_bytes_sent(),
            was_upgraded = session.was_upgraded(),
            error = ?error,
            "spike.done"
        );
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn main() {
    init_tracing("info");
    let listen = env_or("SPIKE_LISTEN", "127.0.0.1:18080");
    let upstream = env_or("SPIKE_UPSTREAM", "127.0.0.1:18081");
    let grace_raw = env_or("SPIKE_GRACE", "3");
    let grace: Option<u64> = if grace_raw == "default" {
        None
    } else {
        grace_raw.parse().ok()
    };
    let shutdown_timeout: u64 = env_or("SPIKE_SHUTDOWN_TIMEOUT", "5").parse().unwrap_or(5);
    let threads: usize = env_or("SPIKE_THREADS", "1").parse().unwrap_or(1);

    let mut conf = ServerConf::default();
    conf.threads = threads;
    conf.daemon = false;
    // Pingora's default grace period is 5 minutes (EXIT_TIMEOUT in
    // pingora-core/src/server/mod.rs); shorten it so the spike can finish.
    conf.grace_period_seconds = grace;
    conf.graceful_shutdown_timeout_seconds = Some(shutdown_timeout);

    let mut server = Server::new_with_opt_and_conf(None, conf);
    server.bootstrap();

    // Cleartext HTTP/2 is opt-in: pingora-core/src/apps/mod.rs gates the h2c
    // preface check on `HttpServerOptions::h2c`, which defaults to false.
    // `HttpServerOptions` is non_exhaustive, so default + field assignment.
    let mut server_options = HttpServerOptions::default();
    server_options.h2c = true;
    let mut service = ProxyServiceBuilder::new(&server.configuration, SpikeProxy::new(upstream.clone()))
        .name("spike-http")
        .server_options(server_options)
        .build();
    service.add_tcp(&listen);
    server.add_service(service);

    info!(
        t_ms = now_ms(),
        pid = std::process::id(),
        listen = %listen,
        upstream = %upstream,
        grace_period_s = ?grace,
        shutdown_timeout_s = shutdown_timeout,
        threads,
        "spike.server.start"
    );
    server.run_forever();
}
