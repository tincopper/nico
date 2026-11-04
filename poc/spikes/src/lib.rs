//! Shared helpers for the Pingora capability spikes.
//!
//! One-shot PoC code. Not a contract, not a library, not shipped.

use std::io;
use std::sync::OnceLock;
use std::time::Instant;

use bytes::{Buf, Bytes, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing_subscriber::EnvFilter;

static START: OnceLock<Instant> = OnceLock::new();

/// Install a stderr tracing subscriber and start the shared monotonic clock.
pub fn init_tracing(level: &str) {
    START.get_or_init(Instant::now);
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(false)
        .try_init();
}

/// Milliseconds since process start. Used to correlate the two spike processes.
pub fn now_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// A parsed HTTP/1 request head.
#[derive(Debug, Clone)]
pub struct Head {
    pub method: String,
    pub target: String,
    pub version: String,
    pub headers: Vec<(String, String)>,
}

impl Head {
    pub fn path(&self) -> &str {
        let path = self.target.split('?').next().unwrap_or(&self.target);
        // Absolute-form request targets (`http://host/path`) are tolerated.
        if let Some(rest) = path.strip_prefix("http://") {
            rest.find('/').map_or("/", |i| &rest[i..])
        } else {
            path
        }
    }

    pub fn query(&self) -> &str {
        self.target
            .split_once('?')
            .map(|(_, q)| q)
            .unwrap_or_default()
    }

    /// Case-insensitive header lookup, first match wins.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Case-insensitive header lookup for repeated headers.
    pub fn header_values(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect()
    }

    pub fn query_u64(&self, key: &str, default: u64) -> u64 {
        self.query()
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .find(|(k, _)| *k == key)
            .and_then(|(_, v)| v.parse().ok())
            .unwrap_or(default)
    }

    pub fn query_str(&self, key: &str, default: &str) -> String {
        self.query()
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.to_string())
            .unwrap_or_else(|| default.to_string())
    }
}

/// A buffered HTTP/1 connection: leftover bytes after a head stay in `buf`.
pub struct Conn {
    pub stream: TcpStream,
    pub buf: BytesMut,
}

impl Conn {
    pub fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            buf: BytesMut::with_capacity(16 * 1024),
        }
    }

    pub fn peer_port(&self) -> u16 {
        self.stream.peer_addr().map(|a| a.port()).unwrap_or(0)
    }

    pub async fn fill(&mut self) -> io::Result<usize> {
        let n = self.stream.read_buf(&mut self.buf).await?;
        Ok(n)
    }

    /// Read one request head. `Ok(None)` means the peer closed cleanly.
    pub async fn read_head(&mut self) -> io::Result<Option<Head>> {
        loop {
            if let Some(pos) = find_double_crlf(&self.buf) {
                let head_bytes = self.buf.split_to(pos + 4).freeze();
                let text = String::from_utf8_lossy(&head_bytes);
                let mut lines = text.split("\r\n");
                let start = lines.next().unwrap_or_default();
                let mut parts = start.split_whitespace();
                let method = parts.next().unwrap_or_default().to_string();
                let target = parts.next().unwrap_or_default().to_string();
                let version = parts.next().unwrap_or_default().to_string();
                let mut headers = Vec::new();
                for line in lines {
                    if line.is_empty() {
                        continue;
                    }
                    if let Some((k, v)) = line.split_once(':') {
                        headers.push((k.trim().to_string(), v.trim().to_string()));
                    }
                }
                return Ok(Some(Head {
                    method,
                    target,
                    version,
                    headers,
                }));
            }
            let n = self.fill().await?;
            if n == 0 {
                return Ok(None);
            }
            if self.buf.len() > 1024 * 1024 {
                return Err(io::Error::other("request head too large"));
            }
        }
    }

    /// Read the full request body (content-length or chunked; trailers discarded).
    pub async fn read_body(&mut self, head: &Head) -> io::Result<Bytes> {
        let chunked = head
            .header("transfer-encoding")
            .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
        if chunked {
            return self.read_chunked().await;
        }
        let len: usize = head
            .header("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        self.read_exact_bytes(len).await
    }

    async fn read_exact_bytes(&mut self, len: usize) -> io::Result<Bytes> {
        while self.buf.len() < len {
            let n = self.fill().await?;
            if n == 0 {
                return Err(io::Error::other(format!(
                    "eof with {} of {} body bytes",
                    self.buf.len(),
                    len
                )));
            }
        }
        Ok(self.buf.split_to(len).freeze())
    }

    async fn read_line(&mut self) -> io::Result<Bytes> {
        loop {
            if let Some(pos) = find_crlf(&self.buf) {
                let line = self.buf.split_to(pos + 2).freeze();
                return Ok(line);
            }
            let n = self.fill().await?;
            if n == 0 {
                return Err(io::Error::other("eof inside chunk framing"));
            }
        }
    }

    async fn read_chunked(&mut self) -> io::Result<Bytes> {
        let mut out = BytesMut::new();
        loop {
            let line = self.read_line().await?;
            let text = String::from_utf8_lossy(&line);
            let size_text = text.trim().split(';').next().unwrap_or("0");
            let size = usize::from_str_radix(size_text.trim(), 16)
                .map_err(|e| io::Error::other(format!("bad chunk size {size_text:?}: {e}")))?;
            if size == 0 {
                // Consume the trailing CRLF (no trailer support; spikes do not send any).
                let _ = self.read_line().await?;
                return Ok(out.freeze());
            }
            let chunk = self.read_exact_bytes(size).await?;
            out.extend_from_slice(&chunk);
            let _ = self.read_line().await?;
        }
    }

    pub async fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        self.stream.write_all(data).await
    }

    pub async fn flush(&mut self) -> io::Result<()> {
        self.stream.flush().await
    }

    /// Write a response head. `body_len: None` means chunked framing follows.
    pub async fn write_head(
        &mut self,
        status: u16,
        reason: &str,
        headers: &[(&str, String)],
        body_len: Option<usize>,
    ) -> io::Result<()> {
        let mut out = format!("HTTP/1.1 {status} {reason}\r\n");
        for (k, v) in headers {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        match body_len {
            Some(n) => out.push_str(&format!("content-length: {n}\r\n")),
            None => out.push_str("transfer-encoding: chunked\r\n"),
        }
        out.push_str("\r\n");
        self.write_all(out.as_bytes()).await
    }

    pub async fn write_chunk(&mut self, data: &[u8]) -> io::Result<()> {
        self.write_all(format!("{:x}\r\n", data.len()).as_bytes()).await?;
        self.write_all(data).await?;
        self.write_all(b"\r\n").await
    }

    pub async fn write_chunk_fin(&mut self) -> io::Result<()> {
        self.write_all(b"0\r\n\r\n").await
    }
}

pub fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

pub fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(data);
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// `Sec-WebSocket-Accept` value for a `Sec-WebSocket-Key` (RFC 6455, 4.2.2).
pub fn ws_accept(key: &str) -> String {
    let mut input = key.trim().as_bytes().to_vec();
    input.extend_from_slice(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    base64_encode(&sha1(&input))
}

/// Read one client WebSocket frame (masked, per RFC 6455 client rules).
pub async fn read_ws_frame(conn: &mut Conn) -> io::Result<(u8, bool, Vec<u8>)> {
    let mut hdr = [0u8; 2];
    read_ws_exact(conn, &mut hdr).await?;
    let fin = hdr[0] & 0x80 != 0;
    let opcode = hdr[0] & 0x0f;
    let masked = hdr[1] & 0x80 != 0;
    let mut len = (hdr[1] & 0x7f) as u64;
    if len == 126 {
        let mut ext = [0u8; 2];
        read_ws_exact(conn, &mut ext).await?;
        len = u16::from_be_bytes(ext) as u64;
    } else if len == 127 {
        let mut ext = [0u8; 8];
        read_ws_exact(conn, &mut ext).await?;
        len = u64::from_be_bytes(ext);
    }
    let mask = if masked {
        let mut m = [0u8; 4];
        read_ws_exact(conn, &mut m).await?;
        Some(m)
    } else {
        None
    };
    let mut payload = vec![0u8; len as usize];
    read_ws_exact(conn, &mut payload).await?;
    if let Some(mask) = mask {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask[i % 4];
        }
    }
    Ok((opcode, fin, payload))
}

async fn read_ws_exact(conn: &mut Conn, out: &mut [u8]) -> io::Result<()> {
    let mut filled = 0;
    while filled < out.len() {
        if conn.buf.is_empty() {
            let n = conn.fill().await?;
            if n == 0 {
                return Err(io::Error::other("eof inside websocket frame"));
            }
        }
        let take = (out.len() - filled).min(conn.buf.len());
        out[filled..filled + take].copy_from_slice(&conn.buf[..take]);
        conn.buf.advance(take);
        filled += take;
    }
    Ok(())
}

/// Write one unmasked server WebSocket frame.
pub async fn write_ws_frame(
    conn: &mut Conn,
    opcode: u8,
    payload: &[u8],
) -> io::Result<()> {
    let mut out = Vec::with_capacity(payload.len() + 10);
    out.push(0x80 | opcode);
    if payload.len() < 126 {
        out.push(payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        out.push(126);
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        out.push(127);
        out.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    out.extend_from_slice(payload);
    conn.write_all(&out).await?;
    conn.flush().await
}

/// Minimal SHA-1 (only used to derive the WebSocket accept value).
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[4 * i],
                chunk[4 * i + 1],
                chunk[4 * i + 2],
                chunk[4 * i + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, v) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_accept_matches_rfc6455_example() {
        // RFC 6455 section 1.3.
        assert_eq!(
            ws_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn base64_pads_correctly() {
        assert_eq!(base64_encode(b"a"), "YQ==");
        assert_eq!(base64_encode(b"ab"), "YWI=");
        assert_eq!(base64_encode(b"abc"), "YWJj");
    }
}
