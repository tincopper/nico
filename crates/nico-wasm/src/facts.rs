//! The facts one stage hands a module, serialized by the host.

use serde::Serialize;

/// The facts the `on_request` stage carries: the client's request head as the gateway sees it.
#[derive(Debug, Serialize)]
pub struct RequestFacts<'a> {
    /// The HTTP method.
    pub method: &'a str,
    /// The client's original request path, without the query.
    pub path: &'a str,
    /// The request headers, in the order the client wrote them.
    pub headers: &'a [(String, String)],
    /// The authenticated subject, when an authentication plugin established one.
    pub subject: Option<&'a str>,
}

/// The facts the `on_response` stage carries: the upstream's response head.
#[derive(Debug, Serialize)]
pub struct ResponseFacts<'a> {
    /// The status the upstream answered with.
    pub status: u16,
    /// The response headers, in upstream order.
    pub headers: &'a [(String, String)],
}
