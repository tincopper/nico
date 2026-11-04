//! The bytes of the mock upstream's protocol: heads, frame reading and response rendering.
//!
//! Keeping this apart from the scripted behaviour in [`super`] means the wire format can be read on
//! its own: the mock writes exactly what a real origin writes — including the chunked framing an
//! event stream needs — and reads whatever framing the proxy chose for the request.

use std::io;

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

/// A connection that keeps the bytes it has read but not consumed.
pub(super) struct Wire {
    stream: TcpStream,
    buffer: Vec<u8>,
}

impl Wire {
    /// Wraps an accepted connection.
    pub(super) fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            buffer: Vec::new(),
        }
    }

    /// Reads one complete head, or `None` when the peer closed before one arrived.
    pub(super) async fn read_head(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            if let Some(end) = find(&self.buffer, b"\r\n\r\n") {
                let head: Vec<u8> = self.buffer.drain(..end + 4).collect();
                return Ok(Some(head));
            }
            if self.pull().await? == 0 {
                return Ok(None);
            }
        }
    }

    /// Reads exactly `length` bytes, recording each piece with its arrival instant.
    pub(super) async fn read_exact(
        &mut self,
        length: usize,
        chunks: &mut Vec<(std::time::Instant, Vec<u8>)>,
    ) -> io::Result<()> {
        let mut remaining = length;
        while remaining > 0 {
            if self.buffer.is_empty() && self.pull().await? == 0 {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
            let take = remaining.min(self.buffer.len());
            let piece: Vec<u8> = self.buffer.drain(..take).collect();
            remaining -= take;
            chunks.push((std::time::Instant::now(), piece));
        }
        Ok(())
    }

    /// Reads a `Transfer-Encoding: chunked` body, recording every data chunk.
    pub(super) async fn read_chunked(
        &mut self,
        chunks: &mut Vec<(std::time::Instant, Vec<u8>)>,
    ) -> io::Result<()> {
        loop {
            let line = self.read_line().await?;
            let text = String::from_utf8_lossy(&line);
            let size = text
                .trim()
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_owned();
            let size = usize::from_str_radix(&size, 16)
                .map_err(|_error| io::Error::from(io::ErrorKind::InvalidData))?;
            if size == 0 {
                // Trailers, terminated by an empty line.
                loop {
                    if self.read_line().await? == b"\r\n" {
                        return Ok(());
                    }
                }
            }
            self.read_exact(size, chunks).await?;
            if self.read_line().await? != b"\r\n" {
                return Err(io::Error::from(io::ErrorKind::InvalidData));
            }
        }
    }

    /// Reads one CRLF-terminated line, including the terminator.
    async fn read_line(&mut self) -> io::Result<Vec<u8>> {
        loop {
            if let Some(end) = find(&self.buffer, b"\r\n") {
                return Ok(self.buffer.drain(..end + 2).collect());
            }
            if self.pull().await? == 0 {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
        }
    }

    /// Reads until the peer closes, returning what was left unread.
    pub(super) async fn read_to_end(&mut self) -> io::Result<Vec<u8>> {
        let mut body = std::mem::take(&mut self.buffer);
        loop {
            match self.pull().await {
                Ok(0) => return Ok(body),
                Ok(_count) => body.append(&mut self.buffer),
                // A reset is the peer closing too; the bytes already read are what matters.
                Err(error) if error.kind() == io::ErrorKind::ConnectionReset => return Ok(body),
                Err(error) => return Err(error),
            }
        }
    }

    /// Writes every byte of `bytes`.
    pub(super) async fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.stream.write_all(bytes).await?;
        self.stream.flush().await
    }

    /// Appends one socket read to the buffer and reports how many bytes arrived.
    async fn pull(&mut self) -> io::Result<usize> {
        let mut scratch = vec![0u8; 8 * 1024];
        let count = self.stream.read(&mut scratch).await?;
        self.buffer.extend_from_slice(&scratch[..count]);
        Ok(count)
    }
}

/// Parses a request head into lowercase header pairs, skipping the request line.
pub(super) fn parse_headers(head: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(head);
    let mut headers = Vec::new();
    for line in text.split("\r\n").skip(1) {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    headers
}

/// Returns the first value of `name` in a parsed header list.
pub(super) fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(existing, _value)| existing.eq_ignore_ascii_case(name))
        .map(|(_name, value)| value.as_str())
}

/// Renders a chunked `200` head, which is what an event stream uses.
pub(super) fn chunked_head() -> Vec<u8> {
    render_head(
        200,
        &[
            ("content-type", "text/event-stream"),
            ("transfer-encoding", "chunked"),
            ("connection", "close"),
            ("x-mock-upstream", "yes"),
        ],
    )
}

/// Writes one chunk of a chunked response.
pub(super) async fn write_chunk(wire: &mut Wire, data: &[u8]) -> io::Result<()> {
    wire.write(format!("{:x}\r\n", data.len()).as_bytes())
        .await?;
    wire.write(data).await?;
    wire.write(b"\r\n").await
}

/// Renders a `200` head that declares `content_length` body bytes.
fn response_head(content_length: usize) -> Vec<u8> {
    let length = content_length.to_string();
    render_head(
        200,
        &[
            ("content-type", "application/octet-stream"),
            ("content-length", length.as_str()),
            ("connection", "close"),
            ("x-mock-upstream", "yes"),
        ],
    )
}

/// Renders one response head with the given status and headers.
///
/// The interim response carries only the headers named here, so the client-side assertion can state
/// exactly what a `1xx` is allowed to contain.
pub(super) fn render_head(status: u16, headers: &[(&str, &str)]) -> Vec<u8> {
    let reason = match status {
        100 => "Continue",
        200 => "OK",
        _ => "",
    };
    let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    head.into_bytes()
}

/// Writes a complete `200` response: head, then the body in `chunk`-sized pieces.
pub(super) async fn write_response(wire: &mut Wire, body: &[u8], chunk: usize) -> io::Result<()> {
    wire.write(&response_head(body.len())).await?;
    for piece in body.chunks(chunk.max(1)) {
        wire.write(piece).await?;
    }
    Ok(())
}

/// Returns the position of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
