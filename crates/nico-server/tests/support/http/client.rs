//! A hand-written HTTP/1.1 client, so the tests assert on wire bytes rather than on a library.
//!
//! The client deliberately supports only what the scenarios need: one request per connection, an
//! explicit `Host`, `Content-Length` or chunked request framing, and a response body framed by
//! `Content-Length`, chunked encoding, or the connection closing. Anything richer would be a second
//! implementation of HTTP to maintain — and would hide the framing the tests exist to verify.
//!
//! Every read and write is bounded by a caller-supplied budget: a test that hangs is worse than one
//! that fails, and a proxy bug shows up as "no answer within the budget", which is a diagnosis. A
//! peer that closes mid-stream ends the body instead of panicking, because a shutdown test observes
//! exactly that.
//!
//! The request and response vocabulary lives next to this module: [`request`] describes what is
//! sent, [`response`] what came back.

mod request;
mod response;

use std::{
    io,
    net::SocketAddr,
    time::{Duration, Instant},
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};

// Re-exported for the scenario files, which name the request they send and the response they
// assert on; every one of these is also used inside this module, so the import is never unused.
pub(crate) use self::{
    request::{Body, Request},
    response::{Head, Response},
};

/// How long one client read may block before the test fails.
pub(crate) const READ_BUDGET: Duration = Duration::from_secs(20);

/// One HTTP/1.1 connection.
pub(crate) struct Client {
    stream: TcpStream,
    /// Bytes read from the socket that the parser has not consumed yet.
    buffer: Vec<u8>,
}

impl Client {
    /// Opens a connection to `address`.
    pub(crate) async fn connect(address: SocketAddr) -> Self {
        let stream = TcpStream::connect(address)
            .await
            .expect("the gateway accepts a connection");
        Self {
            stream,
            buffer: Vec::new(),
        }
    }

    /// Writes the request head, leaving the body to a later [`Self::write_body`].
    pub(crate) async fn write_head(&mut self, request: &Request) -> io::Result<()> {
        let mut head = format!("{} {} HTTP/1.1\r\n", request.method(), request.path());
        for (name, value) in request.headers() {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        match request.framing() {
            Body::None => {}
            Body::Fixed { bytes, .. } => {
                head.push_str(&format!("content-length: {}\r\n", bytes.len()));
            }
            Body::Chunked { .. } => head.push_str("transfer-encoding: chunked\r\n"),
        }
        head.push_str("\r\n");
        self.write_bytes(head.as_bytes()).await
    }

    /// Writes one `Transfer-Encoding: chunked` chunk.
    pub(crate) async fn write_chunk(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.write_bytes(format!("{:x}\r\n", bytes.len()).as_bytes())
            .await?;
        self.write_bytes(bytes).await?;
        self.write_bytes(b"\r\n").await
    }

    /// Writes raw bytes, for a test that chooses its own piece boundaries.
    pub(crate) async fn write_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.stream.write_all(bytes).await?;
        self.stream.flush().await
    }

    /// Writes the body with the framing the request declared.
    pub(crate) async fn write_body(&mut self, body: &Body) -> io::Result<()> {
        match body {
            Body::None => Ok(()),
            Body::Fixed { bytes, chunk } => {
                for piece in bytes.chunks((*chunk).max(1)) {
                    self.write_bytes(piece).await?;
                }
                Ok(())
            }
            Body::Chunked { bytes, chunk } => {
                for piece in bytes.chunks((*chunk).max(1)) {
                    self.write_chunk(piece).await?;
                }
                self.write_bytes(b"0\r\n\r\n").await
            }
        }
    }

    /// Writes the head and the body of `request`.
    pub(crate) async fn write_request(&mut self, request: &Request) -> io::Result<()> {
        self.write_head(request).await?;
        self.write_body(request.framing()).await
    }

    /// Reads one response head, whatever its status.
    ///
    /// An informational response is returned as-is: the `Expect: 100-continue` scenario has to
    /// inspect the `100` the client received, so skipping it here would hide the subject.
    pub(crate) async fn read_head(&mut self, budget: Duration) -> Head {
        loop {
            if let Some(end) = find(&self.buffer, b"\r\n\r\n") {
                let raw: Vec<u8> = self.buffer.drain(..end + 4).collect();
                return Head::parse(&raw);
            }
            if self.fill(budget).await == 0 {
                panic!(
                    "the connection closed before a response head; {} bytes were received",
                    self.buffer.len()
                );
            }
        }
    }

    /// Reads the response body, framed by `Content-Length`, chunked encoding, or the closing peer.
    pub(crate) async fn read_body(&mut self, head: &Head, budget: Duration) -> Vec<u8> {
        let chunks = self.read_chunks(head, budget).await;
        let mut body = Vec::new();
        for (_at, bytes) in chunks {
            body.extend_from_slice(&bytes);
        }
        body
    }

    /// Reads the body, keeping each chunk's arrival instant.
    ///
    /// For a chunked response the instants are those of the decoded chunks, which is what makes an
    /// event stream measurable; for a `Content-Length` response they are the reads that completed
    /// the remaining length.
    pub(crate) async fn read_chunks(
        &mut self,
        head: &Head,
        budget: Duration,
    ) -> Vec<(Instant, Vec<u8>)> {
        if head.body_forbidden() {
            return Vec::new();
        }
        if head.is_chunked() {
            let mut chunks = Vec::new();
            while let Some(data) = self.next_chunk(budget).await {
                chunks.push((Instant::now(), data));
            }
            return chunks;
        }

        let mut chunks = Vec::new();
        let mut remaining = head.content_length();
        if !self.buffer.is_empty() {
            let buffered = std::mem::take(&mut self.buffer);
            if let Some(left) = remaining.as_mut() {
                *left = left.saturating_sub(buffered.len());
            }
            chunks.push((Instant::now(), buffered));
        }
        while remaining != Some(0) {
            if self.fill(budget).await == 0 {
                break;
            }
            let bytes = std::mem::take(&mut self.buffer);
            if let Some(left) = remaining.as_mut() {
                *left = left.saturating_sub(bytes.len());
            }
            chunks.push((Instant::now(), bytes));
        }
        chunks
    }

    /// Reads the next decoded chunk of a chunked response, or `None` when the stream ended.
    ///
    /// Unlike [`Self::read_chunks`], this returns as soon as one chunk is complete, which is what a
    /// test that acts *during* a stream needs. A peer that closes mid-frame ends the stream rather
    /// than corrupting it: a partial frame is not returned.
    pub(crate) async fn next_chunk(&mut self, budget: Duration) -> Option<Vec<u8>> {
        let line = self.read_line(budget).await?;
        let text = String::from_utf8_lossy(&line);
        let size = text
            .trim()
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned();
        let size = usize::from_str_radix(&size, 16)
            .unwrap_or_else(|_error| panic!("the chunk size {size:?} is not hexadecimal"));
        if size == 0 {
            // Trailers, terminated by an empty line.
            while self
                .read_line(budget)
                .await
                .is_some_and(|line| line != b"\r\n")
            {}
            return None;
        }
        let data = self.read_exact(size, budget).await?;
        let terminator = self.read_line(budget).await?;
        assert_eq!(
            terminator, b"\r\n",
            "a chunk must be terminated by CRLF, got {terminator:?}"
        );
        Some(data)
    }

    /// Reads exactly one CRLF-terminated line, including its terminator.
    ///
    /// `None` means the peer closed before the line was complete.
    async fn read_line(&mut self, budget: Duration) -> Option<Vec<u8>> {
        loop {
            if let Some(end) = find(&self.buffer, b"\r\n") {
                return Some(self.buffer.drain(..end + 2).collect());
            }
            if self.fill(budget).await == 0 {
                return None;
            }
        }
    }

    /// Reads exactly `length` bytes.
    ///
    /// `None` means the peer closed first, which cuts the stream short rather than corrupting it.
    async fn read_exact(&mut self, length: usize, budget: Duration) -> Option<Vec<u8>> {
        let mut bytes = Vec::with_capacity(length);
        while bytes.len() < length {
            if self.buffer.is_empty() && self.fill(budget).await == 0 {
                return None;
            }
            let take = (length - bytes.len()).min(self.buffer.len());
            bytes.extend(self.buffer.drain(..take));
        }
        Some(bytes)
    }

    /// Appends one socket read to the buffer and reports how many bytes arrived.
    ///
    /// Zero means the peer closed. A reset is treated the same way: a forced shutdown closes the
    /// socket abruptly, and the bytes already received are still the observation a test needs.
    async fn fill(&mut self, budget: Duration) -> usize {
        let mut scratch = vec![0u8; 64 * 1024];
        let read = timeout(budget, self.stream.read(&mut scratch))
            .await
            .unwrap_or_else(|_elapsed| panic!("the peer sent nothing within {budget:?}"));
        match read {
            Ok(count) => {
                self.buffer.extend_from_slice(&scratch[..count]);
                count
            }
            Err(error) if error.kind() == io::ErrorKind::ConnectionReset => 0,
            Err(error) => panic!("reading from the peer failed: {error}"),
        }
    }
}

/// Sends `request` on a fresh connection and reads the complete response.
///
/// An informational response is skipped here: only the scenario that inspects a `100` drives the
/// client by hand.
pub(crate) async fn exchange(address: SocketAddr, request: &Request) -> Response {
    let mut client = Client::connect(address).await;
    client
        .write_request(request)
        .await
        .expect("the request is written");
    let mut head = client.read_head(READ_BUDGET).await;
    while (100..200).contains(&head.status()) {
        head = client.read_head(READ_BUDGET).await;
    }
    let body = client.read_body(&head, READ_BUDGET).await;
    Response::new(head, body)
}

/// Performs one `GET` on a fresh connection and reads the complete response.
pub(crate) async fn get(address: SocketAddr, path: &str, host: &str) -> Response {
    exchange(address, &Request::get(path, host)).await
}

/// Returns the position of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
