//! The framework-free facts of one HTTP exchange, as a codec or an adapter sees them.
//!
//! A codec may need more than the body (a protocol can carry its model in the path or its version
//! in a header), and an adapter must interpret a status before the body is read. Both are owned
//! values with no framework type in them, so a codec and an adapter are testable without a socket
//! and the data plane never leaks its header map into this contract.
//!
//! Every value is redacted in `Debug`. A header value can carry a credential — a client's
//! `authorization` on the way in, a provider's `set-cookie` on the way out — so a record names the
//! headers and leaves what they hold to the accessors.

use std::fmt;

/// The owned, framework-free facts of a client request.
///
/// A codec may need more than the body: a protocol can carry its model in the path or its version in
/// a header, so the data plane hands over what it has before the body is decoded. The values are
/// owned instead of borrowed from a session because a codec must be usable from a test and because
/// the exchange outlives the hook that produced it.
#[derive(Clone, PartialEq, Eq)]
pub struct RequestFacts {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
}

impl fmt::Debug for RequestFacts {
    /// Names the headers and never prints their values: a value can carry a credential.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RequestFacts")
            .field("method", &self.method)
            .field("path", &self.path)
            .field("headers", &header_names(&self.headers))
            .finish()
    }
}

impl RequestFacts {
    /// Builds the facts of one request.
    #[must_use]
    pub fn new(
        method: impl Into<String>,
        path: impl Into<String>,
        headers: Vec<(String, String)>,
    ) -> Self {
        Self {
            method: method.into(),
            path: path.into(),
            headers,
        }
    }

    /// Returns the request method.
    #[must_use]
    pub fn method(&self) -> &str {
        &self.method
    }

    /// Returns the request path including any query string.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Returns the headers in the order the client sent them.
    #[must_use]
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// Returns the first value of `name`, compared ASCII-case-insensitively.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        first_header(&self.headers, name)
    }
}

/// The owned, framework-free facts of an upstream response.
///
/// A provider adapter reads the status and headers to decide whether the body is a success document
/// or an error one, and a stream decoder needs the media type before the first chunk. Like
/// [`RequestFacts`], the value is owned so an adapter is testable without a socket.
#[derive(Clone, PartialEq, Eq)]
pub struct ResponseFacts {
    status: u16,
    headers: Vec<(String, String)>,
}

impl fmt::Debug for ResponseFacts {
    /// Names the headers and never prints their values: a provider can echo caller data in them.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponseFacts")
            .field("status", &self.status)
            .field("headers", &header_names(&self.headers))
            .finish()
    }
}

impl ResponseFacts {
    /// Builds the facts of one upstream response.
    #[must_use]
    pub fn new(status: u16, headers: Vec<(String, String)>) -> Self {
        Self { status, headers }
    }

    /// Returns the status the provider answered with.
    #[must_use]
    pub const fn status(&self) -> u16 {
        self.status
    }

    /// Returns the headers in the order the provider sent them.
    #[must_use]
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// Returns the first value of `name`, compared ASCII-case-insensitively.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        first_header(&self.headers, name)
    }

    /// Reports whether the status is a 2xx success.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        self.status >= 200 && self.status <= 299
    }
}

/// Returns the first value of `name`, compared ASCII-case-insensitively.
fn first_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// Collects header names for a `Debug` record.
///
/// A header value can carry a credential (a client's `authorization`, a provider's `set-cookie`), so
/// a debug record names the headers and never prints what they hold. Accessors remain the way to
/// read a value.
pub(crate) fn header_names(headers: &[(String, String)]) -> Vec<&str> {
    headers.iter().map(|(name, _)| name.as_str()).collect()
}
