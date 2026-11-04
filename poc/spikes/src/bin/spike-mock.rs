//! Hand-written TCP mock upstream for the Pingora capability spikes.
//!
//! Deliberately no hyper/axum: the point is to observe raw bytes on the wire
//! (connections, framing, inter-write timing) without a framework in the way.
//! HTTP/2 is spoken with the `h2` protocol state machine so gRPC trailers can
//! be produced; every HTTP/1.x behaviour is hand-written.

use std::io;
use std::time::Instant;

use bytes::Bytes;
use spikes::{
    Conn, Head, init_tracing, now_ms, read_ws_frame, sha256_hex, write_ws_frame, ws_accept,
};
use tokio::net::{TcpListener, TcpStream};
use tracing::info;

const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

#[tokio::main]
async fn main() -> io::Result<()> {
    init_tracing("info");
    let addr = std::env::var("SPIKE_MOCK_ADDR").unwrap_or_else(|_| "127.0.0.1:18081".into());
    let listener = TcpListener::bind(&addr).await?;
    info!(t_ms = now_ms(), addr = %addr, "mock.listen");
    loop {
        let (stream, peer) = listener.accept().await?;
        let local = stream.local_addr()?.port();
        info!(
            t_ms = now_ms(),
            local_port = local,
            peer_port = peer.port(),
            "mock.conn.accepted"
        );
        tokio::spawn(async move {
            if let Err(e) = serve(stream, local).await {
                info!(
                    t_ms = now_ms(),
                    local_port = local,
                    error = %e,
                    "mock.conn.error"
                );
            }
        });
    }
}

async fn serve(stream: TcpStream, local_port: u16) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let mut conn = Conn::new(stream);
    let is_h2 = detect_h2(&mut conn).await?;
    if is_h2 {
        serve_h2(conn.stream, local_port).await
    } else {
        serve_h1(conn, local_port).await
    }
}

/// Peek far enough to tell the HTTP/2 connection preface from an HTTP/1 request line.
async fn detect_h2(conn: &mut Conn) -> io::Result<bool> {
    loop {
        let mut probe = [0u8; 24];
        let n = conn.stream.peek(&mut probe).await?;
        if n == 0 {
            return Ok(false);
        }
        let head = &probe[..n];
        if H2_PREFACE.starts_with(head) {
            if n >= H2_PREFACE.len() {
                return Ok(true);
            }
            // Partial preface: give the client a moment to finish writing it.
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            continue;
        }
        return Ok(false);
    }
}

async fn serve_h1(mut conn: Conn, local_port: u16) -> io::Result<()> {
    loop {
        let Some(head) = conn.read_head().await? else {
            info!(
                t_ms = now_ms(),
                local_port,
                peer_port = conn.peer_port(),
                "mock.conn.eof"
            );
            return Ok(());
        };
        info!(
            t_ms = now_ms(),
            local_port,
            peer_port = conn.peer_port(),
            method = %head.method,
            target = %head.target,
            version = %head.version,
            "mock.req.head"
        );
        // Answer `Expect: 100-continue` the way a real origin would, so the
        // proxy's handling of informational responses is observable.
        if head
            .header("expect")
            .is_some_and(|v| v.to_ascii_lowercase().contains("100-continue"))
        {
            conn.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await?;
            conn.flush().await?;
            info!(
                t_ms = now_ms(),
                local_port,
                peer_port = conn.peer_port(),
                "mock.req.100_continue_sent"
            );
        }
        let body = conn.read_body(&head).await?;
        info!(
            t_ms = now_ms(),
            local_port,
            peer_port = conn.peer_port(),
            target = %head.target,
            body_bytes = body.len(),
            body_sha256 = %sha256_hex(&body),
            body_head = %String::from_utf8_lossy(&body[..body.len().min(48)]),
            "mock.req.body"
        );

        let keep_alive = match head.path() {
            "/hello" => hello(&mut conn, local_port).await?,
            "/upload" => upload(&mut conn, &body).await?,
            "/download" => download(&mut conn, &head).await?,
            "/sse" => sse(&mut conn, &head, false).await?,
            "/sse-frag" => sse(&mut conn, &head, true).await?,
            "/flood" => flood(&mut conn, &head).await?,
            "/cancel" => cancel(&mut conn).await?,
            "/pool" => pool(&mut conn, local_port).await?,
            "/ws" => return websocket(&mut conn, &head, local_port).await,
            "/grpc" => {
                // gRPC needs HTTP/2 end-to-end; tell the caller instead of pretending.
                conn.write_head(
                    505,
                    "HTTP Version Not Supported",
                    &[("content-type", "text/plain".into())],
                    Some(9),
                )
                .await?;
                conn.write_all(b"need h2\r\n").await?;
                false
            }
            _ => not_found(&mut conn).await?,
        };
        if !head
            .header("connection")
            .is_some_and(|v| v.eq_ignore_ascii_case("close"))
            && keep_alive
        {
            continue;
        }
        info!(
            t_ms = now_ms(),
            local_port,
            peer_port = conn.peer_port(),
            "mock.conn.close"
        );
        return Ok(());
    }
}

async fn hello(conn: &mut Conn, local_port: u16) -> io::Result<bool> {
    let body = format!("mock-hello local_port={local_port} peer_port={}", conn.peer_port());
    conn.write_head(
        200,
        "OK",
        &[
            ("content-type", "text/plain".into()),
            ("x-mock-peer-port", conn.peer_port().to_string()),
            ("x-mock-server", "spike-mock".into()),
        ],
        Some(body.len()),
    )
    .await?;
    conn.write_all(body.as_bytes()).await?;
    conn.flush().await?;
    Ok(true)
}

async fn upload(conn: &mut Conn, body: &Bytes) -> io::Result<bool> {
    let reply = format!(
        "{{\"len\":{},\"sha256\":\"{}\"}}",
        body.len(),
        sha256_hex(body)
    );
    conn.write_head(
        200,
        "OK",
        &[
            ("content-type", "application/json".into()),
            ("x-mock-body-bytes", body.len().to_string()),
        ],
        Some(reply.len()),
    )
    .await?;
    conn.write_all(reply.as_bytes()).await?;
    conn.flush().await?;
    Ok(true)
}

/// Deterministic body so the client can verify byte-exact forwarding with sha256.
fn download_body(mb: u64) -> Vec<u8> {
    let len = (mb as usize) * 1024 * 1024;
    let mut out = Vec::with_capacity(len);
    let mut x: u32 = 0x1234_5678;
    for _ in 0..len {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        out.push((x >> 24) as u8);
    }
    out
}

async fn download(conn: &mut Conn, head: &Head) -> io::Result<bool> {
    let mb = head.query_u64("mb", 1);
    let body = download_body(mb);
    conn.write_head(
        200,
        "OK",
        &[
            ("content-type", "application/octet-stream".into()),
            ("x-mock-sha256", sha256_hex(&body)),
        ],
        Some(body.len()),
    )
    .await?;
    for chunk in body.chunks(64 * 1024) {
        conn.write_all(chunk).await?;
    }
    conn.flush().await?;
    Ok(true)
}

fn sse_events(n: u64) -> Vec<String> {
    let mut events = Vec::new();
    for i in 0..n {
        events.push(format!("data: event-{i}\n\n"));
    }
    events.push("data: [DONE]\n\n".to_string());
    events
}

/// SSE with either one write per event or byte-level fragmentation of the same bytes.
async fn sse(conn: &mut Conn, head: &Head, fragment: bool) -> io::Result<bool> {
    let n = head.query_u64("n", 5);
    let delay_ms = head.query_u64("delay_ms", 200);
    let mode = head.query_str("mode", "rand");
    conn.write_head(
        200,
        "OK",
        &[
            ("content-type", "text/event-stream".into()),
            ("cache-control", "no-cache".into()),
            ("x-mock-sse-events", n.to_string()),
        ],
        None,
    )
    .await?;
    conn.flush().await?;
    let start = Instant::now();
    for (i, event) in sse_events(n).into_iter().enumerate() {
        if delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }
        let bytes = event.as_bytes().to_vec();
        if fragment {
            let mut offset = 0usize;
            let mut rng: u32 = 0x9E37_79B9;
            while offset < bytes.len() {
                let take = match mode.as_str() {
                    "1" => 1,
                    "3" => 3,
                    _ => {
                        rng = rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        1 + (rng as usize % 7)
                    }
                };
                let end = (offset + take).min(bytes.len());
                conn.write_chunk(&bytes[offset..end]).await?;
                conn.flush().await?;
                offset = end;
            }
        } else {
            conn.write_chunk(&bytes).await?;
            conn.flush().await?;
        }
        info!(
            t_ms = now_ms(),
            event = i,
            event_bytes = bytes.len(),
            elapsed_ms = start.elapsed().as_millis() as u64,
            fragmented = fragment,
            "mock.sse.wrote"
        );
    }
    conn.write_chunk_fin().await?;
    conn.flush().await?;
    info!(
        t_ms = now_ms(),
        events = n + 1,
        elapsed_ms = start.elapsed().as_millis() as u64,
        "mock.sse.done"
    );
    Ok(true)
}

/// Large response written as fast as the socket accepts it: backpressure probe.
async fn flood(conn: &mut Conn, head: &Head) -> io::Result<bool> {
    let mb = head.query_u64("mb", 64);
    let chunk = head.query_u64("chunk_kb", 64) as usize * 1024;
    let total = (mb as usize) * 1024 * 1024;
    conn.write_head(
        200,
        "OK",
        &[
            ("content-type", "application/octet-stream".into()),
            ("x-mock-flood-bytes", total.to_string()),
        ],
        Some(total),
    )
    .await?;
    let block = vec![0x5au8; chunk.max(1)];
    let start = Instant::now();
    let mut written = 0usize;
    let mut next_mark = 8 * 1024 * 1024usize;
    while written < total {
        let take = block.len().min(total - written);
        match conn.write_all(&block[..take]).await {
            Ok(()) => {}
            Err(e) => {
                info!(
                    t_ms = now_ms(),
                    written,
                    total,
                    elapsed_ms = start.elapsed().as_millis() as u64,
                    error = %e,
                    "mock.flood.write_failed"
                );
                return Ok(false);
            }
        }
        written += take;
        if written >= next_mark {
            info!(
                t_ms = now_ms(),
                written,
                total,
                elapsed_ms = start.elapsed().as_millis() as u64,
                "mock.flood.progress"
            );
            next_mark += 8 * 1024 * 1024;
        }
    }
    conn.flush().await?;
    info!(
        t_ms = now_ms(),
        written,
        total,
        elapsed_ms = start.elapsed().as_millis() as u64,
        "mock.flood.done"
    );
    Ok(true)
}

/// Endless SSE-like stream: used to observe client-cancel propagation.
async fn cancel(conn: &mut Conn) -> io::Result<bool> {
    conn.write_head(
        200,
        "OK",
        &[
            ("content-type", "text/event-stream".into()),
            ("cache-control", "no-cache".into()),
        ],
        None,
    )
    .await?;
    conn.flush().await?;
    let start = Instant::now();
    let mut i = 0u64;
    loop {
        let event = format!("data: tick-{i}\n\n");
        match conn.write_chunk(event.as_bytes()).await {
            Ok(()) => {}
            Err(e) => {
                info!(
                    t_ms = now_ms(),
                    ticks = i,
                    elapsed_ms = start.elapsed().as_millis() as u64,
                    error = %e,
                    "mock.cancel.write_failed"
                );
                return Ok(false);
            }
        }
        if let Err(e) = conn.flush().await {
            info!(
                t_ms = now_ms(),
                ticks = i,
                elapsed_ms = start.elapsed().as_millis() as u64,
                error = %e,
                "mock.cancel.flush_failed"
            );
            return Ok(false);
        }
        if i % 10 == 0 {
            info!(
                t_ms = now_ms(),
                tick = i,
                elapsed_ms = start.elapsed().as_millis() as u64,
                "mock.cancel.tick"
            );
        }
        i += 1;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Small response that reveals which upstream TCP connection served it.
async fn pool(conn: &mut Conn, local_port: u16) -> io::Result<bool> {
    let body = format!("peer_port={} local_port={local_port}", conn.peer_port());
    conn.write_head(
        200,
        "OK",
        &[
            ("content-type", "text/plain".into()),
            ("x-mock-peer-port", conn.peer_port().to_string()),
        ],
        Some(body.len()),
    )
    .await?;
    conn.write_all(body.as_bytes()).await?;
    conn.flush().await?;
    Ok(true)
}

async fn not_found(conn: &mut Conn) -> io::Result<bool> {
    conn.write_head(404, "Not Found", &[], Some(0)).await?;
    Ok(true)
}

/// WebSocket upgrade + echo. Logs every frame so the proxy's frame transparency is observable.
async fn websocket(conn: &mut Conn, head: &Head, local_port: u16) -> io::Result<()> {
    let Some(key) = head.header("sec-websocket-key") else {
        conn.write_head(400, "Bad Request", &[], Some(0)).await?;
        return Ok(());
    };
    if !head
        .header("upgrade")
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
    {
        conn.write_head(400, "Bad Request", &[], Some(0)).await?;
        return Ok(());
    }
    let accept = ws_accept(key);
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-accept: {accept}\r\n\r\n"
    );
    conn.write_all(response.as_bytes()).await?;
    conn.flush().await?;
    info!(
        t_ms = now_ms(),
        local_port,
        peer_port = conn.peer_port(),
        accept = %accept,
        "mock.ws.upgraded"
    );
    loop {
        let (opcode, fin, payload) = match read_ws_frame(conn).await {
            Ok(frame) => frame,
            Err(e) => {
                info!(
                    t_ms = now_ms(),
                    local_port,
                    peer_port = conn.peer_port(),
                    error = %e,
                    "mock.ws.eof"
                );
                return Ok(());
            }
        };
        info!(
            t_ms = now_ms(),
            local_port,
            peer_port = conn.peer_port(),
            opcode,
            fin,
            len = payload.len(),
            payload = %String::from_utf8_lossy(&payload),
            "mock.ws.frame.in"
        );
        match opcode {
            0x8 => {
                write_ws_frame(conn, 0x8, &payload).await?;
                info!(t_ms = now_ms(), "mock.ws.close");
                return Ok(());
            }
            0x9 => {
                write_ws_frame(conn, 0xA, &payload).await?;
            }
            _ => {
                write_ws_frame(conn, opcode, &payload).await?;
                info!(
                    t_ms = now_ms(),
                    len = payload.len(),
                    "mock.ws.frame.out"
                );
            }
        }
    }
}

/// HTTP/2 server used for the gRPC-shape probe (headers + DATA + trailers).
async fn serve_h2(stream: TcpStream, local_port: u16) -> io::Result<()> {
    let mut conn = h2::server::handshake(stream).await.map_err(io::Error::other)?;
    while let Some(request) = conn.accept().await {
        let (request, mut respond) = request.map_err(io::Error::other)?;
        let (parts, mut body) = request.into_parts();
        let mut body_len = 0usize;
        while let Some(chunk) = body.data().await {
            let chunk = chunk.map_err(io::Error::other)?;
            body_len += chunk.len();
            let _ = body.flow_control().release_capacity(chunk.len());
        }
        info!(
            t_ms = now_ms(),
            local_port,
            method = %parts.method,
            uri = %parts.uri,
            version = ?parts.version,
            body_bytes = body_len,
            headers = ?parts.headers,
            "mock.h2.req"
        );

        let mut trailers = http::HeaderMap::new();
        trailers.insert("grpc-status", http::HeaderValue::from_static("0"));
        trailers.insert("grpc-message", http::HeaderValue::from_static("spike-ok"));

        if parts.uri.path() == "/grpc" {
            let response = http::Response::builder()
                .status(200)
                .header("content-type", "application/grpc")
                .header("x-mock-h2", "grpc")
                .body(())
                .map_err(io::Error::other)?;
            let mut send = respond.send_response(response, false).map_err(io::Error::other)?;
            for i in 0..2u8 {
                let payload = format!("grpc-payload-{i}");
                let mut frame = Vec::with_capacity(payload.len() + 5);
                frame.push(0);
                frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
                frame.extend_from_slice(payload.as_bytes());
                send.send_data(Bytes::from(frame), false)
                    .map_err(io::Error::other)?;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            send.send_trailers(trailers).map_err(io::Error::other)?;
            info!(t_ms = now_ms(), "mock.h2.grpc.trailers_sent");
        } else {
            let body = format!("mock-h2 path={}", parts.uri.path());
            let response = http::Response::builder()
                .status(200)
                .header("content-type", "text/plain")
                .header("x-mock-h2", "plain")
                .body(())
                .map_err(io::Error::other)?;
            let mut send = respond.send_response(response, false).map_err(io::Error::other)?;
            send.send_data(Bytes::from(body), true)
                .map_err(io::Error::other)?;
        }
        // Returning to accept() drives the h2 connection so the frames above are flushed.
    }
    info!(t_ms = now_ms(), local_port, "mock.h2.conn.closed");
    Ok(())
}
