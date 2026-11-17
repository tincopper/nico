//! A real TLS client for the inbound listener tests.
//!
//! The origin-side helper in [`super::tls`] proves what the gateway does when it *connects* over
//! TLS. This module proves the opposite direction: a client verifies the gateway's listener
//! certificate, negotiates ALPN, and sends one HTTP/1.1 request over the encrypted stream. It uses
//! the same fixture roots, so the handshake either succeeds against the test CA or fails for a
//! reason the test chose.

use std::{
    fs::File,
    io::{self, BufReader},
    net::SocketAddr,
    path::Path,
    sync::Arc,
    time::Duration,
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};
use tokio_rustls::{
    TlsConnector,
    client::TlsStream,
    rustls::{ClientConfig, RootCertStore, pki_types::ServerName},
};

/// How long one client read may block.
pub(crate) const READ_BUDGET: Duration = Duration::from_secs(20);

/// One TLS connection to the gateway listener.
pub(crate) struct TlsClient {
    stream: TlsStream<TcpStream>,
    alpn: Option<Vec<u8>>,
}

impl TlsClient {
    /// Connects to `address`, verifies the listener with `roots`, and offers `alpn` protocols.
    ///
    /// # Errors
    /// Returns the connect/handshake failure. A wrong root or a name the certificate does not carry
    /// fails here, which is how a scenario distinguishes a verified handshake from a refused one.
    pub(crate) async fn connect(
        address: SocketAddr,
        server_name: &str,
        roots: &Path,
        alpn: &[&[u8]],
    ) -> io::Result<Self> {
        let mut store = RootCertStore::empty();
        let mut reader = BufReader::new(File::open(roots)?);
        for certificate in rustls_pemfile::certs(&mut reader) {
            store
                .add(certificate?)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        }

        let mut config = ClientConfig::builder()
            .with_root_certificates(store)
            .with_no_client_auth();
        config.alpn_protocols = alpn.iter().map(|protocol| protocol.to_vec()).collect();

        let connector = TlsConnector::from(Arc::new(config));
        let name = ServerName::try_from(server_name.to_owned())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let tcp = TcpStream::connect(address).await?;
        let stream = connector.connect(name, tcp).await?;
        let alpn = stream.get_ref().1.alpn_protocol().map(<[u8]>::to_vec);
        Ok(Self { stream, alpn })
    }

    /// Returns the ALPN protocol the gateway selected.
    pub(crate) fn alpn(&self) -> Option<&[u8]> {
        self.alpn.as_deref()
    }

    /// Sends one `GET` and returns the raw response bytes.
    ///
    /// `Connection: close` makes the read end at the gateway's response instead of waiting for a
    /// reuse decision. The response is intentionally returned as bytes: the scenario asserts the
    /// status line and body without adding an HTTP parser to the fixture.
    pub(crate) async fn get(&mut self, path: &str, host: &str) -> io::Result<Vec<u8>> {
        let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
        self.stream.write_all(request.as_bytes()).await?;
        self.stream.flush().await?;
        let mut response = Vec::new();
        timeout(READ_BUDGET, self.stream.read_to_end(&mut response))
            .await
            .map_err(|_error| {
                io::Error::new(io::ErrorKind::TimedOut, "TLS response read timed out")
            })??;
        Ok(response)
    }
}
