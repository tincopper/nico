//! One `upstreams` entry of a fixture document.
//!
//! The entry is its own file because it owns the two TLS facts a scenario varies: the scheme, and the
//! host a certificate is verified against. A host that is an *address* rather than a name is the
//! mechanism behind the hostname-mismatch scenario — the peer resolves, the chain verifies and the
//! name does not match — so the mapping from a socket address to `(host, port)` is part of this
//! structure's job rather than a detail of the renderer that happens to hold it.

/// One entry of the document's `upstreams` collection.
#[derive(Debug, Clone)]
pub(super) struct UpstreamEntry {
    name: String,
    scheme: &'static str,
    host: String,
    port: u16,
}

impl UpstreamEntry {
    /// A plaintext entry addressed by `host`:`port`.
    pub(super) fn plaintext(name: &str, host: String, port: u16) -> Self {
        Self {
            name: name.to_owned(),
            scheme: "http",
            host,
            port,
        }
    }

    /// An `https` entry addressed by the name its certificate is signed for.
    pub(super) fn tls(name: &str, host: &str, port: u16) -> Self {
        Self {
            name: name.to_owned(),
            scheme: "https",
            host: host.to_owned(),
            port,
        }
    }

    /// Renders the entry as the lines it occupies under `upstreams:`.
    pub(super) fn render(&self) -> String {
        format!(
            "  - name: {name}\n    scheme: {scheme}\n    host: {host}\n    port: {port}\n",
            name = self.name,
            scheme = self.scheme,
            host = self.host,
            port = self.port,
        )
    }
}
