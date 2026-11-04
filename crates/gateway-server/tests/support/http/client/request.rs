//! The request vocabulary: what the client sends, and how it frames the body.
//!
//! A scenario picks the framing explicitly — no body, a declared `Content-Length`, or chunked
//! encoding — because the proxy must preserve the bytes whatever the framing is. The piece size is
//! a separate choice, so a test can write one byte at a time without changing the framing.

/// Request framing: what the client declares and how it writes the body.
#[derive(Debug, Clone, Default)]
pub(crate) enum Body {
    /// No body and no framing header.
    #[default]
    None,
    /// `Content-Length` framing, written in `chunk`-sized pieces.
    Fixed {
        /// The complete body.
        bytes: Vec<u8>,
        /// How many bytes one write carries.
        chunk: usize,
    },
    /// `Transfer-Encoding: chunked`, written in `chunk`-sized pieces.
    Chunked {
        /// The complete body, before framing.
        bytes: Vec<u8>,
        /// How many bytes one chunk carries.
        chunk: usize,
    },
}

/// One request, exactly as the client will write it.
#[derive(Debug, Clone)]
pub(crate) struct Request {
    method: &'static str,
    path: String,
    headers: Vec<(String, String)>,
    body: Body,
}

impl Request {
    /// A `GET` that addresses `host`.
    pub(crate) fn get(path: &str, host: &str) -> Self {
        Self::new("GET", path, host)
    }

    /// A `POST` that addresses `host`.
    pub(crate) fn post(path: &str, host: &str) -> Self {
        Self::new("POST", path, host)
    }

    /// A request with `host` as its only header.
    pub(crate) fn new(method: &'static str, path: &str, host: &str) -> Self {
        Self {
            method,
            path: path.to_owned(),
            headers: vec![("host".to_owned(), host.to_owned())],
            body: Body::None,
        }
    }

    /// Adds a request header. A later value for the same name replaces the earlier one.
    pub(crate) fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        let value = value.into();
        match self
            .headers
            .iter_mut()
            .find(|(existing, _value)| existing.eq_ignore_ascii_case(name))
        {
            Some((_existing, existing)) => *existing = value,
            None => self.headers.push((name.to_owned(), value)),
        }
        self
    }

    /// Sets a `Content-Length` body, written in `chunk`-sized pieces.
    pub(crate) fn body(mut self, bytes: Vec<u8>, chunk: usize) -> Self {
        self.body = Body::Fixed { bytes, chunk };
        self
    }

    /// Sets a chunked body, written in `chunk`-sized pieces.
    pub(crate) fn chunked(mut self, bytes: Vec<u8>, chunk: usize) -> Self {
        self.body = Body::Chunked { bytes, chunk };
        self
    }

    /// Returns the request method.
    pub(crate) fn method(&self) -> &'static str {
        self.method
    }

    /// Returns the request path, without a query string.
    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    /// Returns the headers as they will be written, without the framing header.
    pub(crate) fn headers(&self) -> impl Iterator<Item = (&str, &str)> {
        self.headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    /// Returns the body and its framing.
    pub(crate) fn framing(&self) -> &Body {
        &self.body
    }
}
