//! Route-level request header rewrites.

/// One request-header rewrite a route declares: the header name, and what to write into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxySetHeader {
    pub(super) name: String,
    pub(super) value: ProxyHeaderValue,
}

impl ProxySetHeader {
    /// Builds an entry from values the parser has already validated.
    pub(super) fn new(name: String, value: ProxyHeaderValue) -> Self {
        Self { name, value }
    }

    /// Returns the header name, lowercase.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the value to write.
    #[must_use]
    pub const fn value(&self) -> &ProxyHeaderValue {
        &self.value
    }
}

/// What one `proxy_set_header` entry writes into the header it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyHeaderValue {
    /// A literal string. An empty value suppresses the header.
    Literal(String),
    /// The client's original `Host`, the equivalent of nginx's `$http_host`.
    ClientHost,
    /// The gateway's default: the upstream's logical host, as the automatic behavior writes it.
    UpstreamHost,
}
