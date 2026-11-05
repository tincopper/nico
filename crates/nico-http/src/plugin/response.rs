//! A response a plugin synthesized, and the cleaning that turns it into bytes the data plane writes.
//!
//! Not every answer is a forward and not every negative answer is a failure: a preflight, a mock, a
//! cache hit are complete responses a plugin already knows. [`Outcome::Respond`](super::Outcome)
//! carries one, and the data plane still writes it — through the same single writer every other
//! answer goes through.
//!
//! Two stages, deliberately separate:
//!
//! 1. [`ResponseData::new`] validates what a plugin may *say*. The status is an HTTP status, and a
//!    status that cannot carry a body cannot be given one; a header name is a token and a value is
//!    visible ASCII (see [`super::header`]), because those strings cross a WASM boundary and reach a
//!    header map that stores names as opaque bytes.
//! 2. [`clean_response`] decides what the data plane may *write*. The plugin's header list is not a
//!    wire head: hop-by-hop headers are dropped, `content-length` is dropped and recomputed from the
//!    body actually written, and the two headers only a refusal may carry (`www-authenticate`,
//!    `retry-after`) are removed so a plugin cannot forge them by returning a response.
//!
//! These are **two different rules**, and they must stay different. A refusal keeps a closed header
//! set (`content-type`, `content-length`, `x-nico-error-code`, `x-nico-trace-id`, plus
//! `www-authenticate` and `retry-after`) because it is a small, typed answer; a synthesized response
//! carries whatever end-to-end headers the plugin produced, cleaned of the ones the data plane owns.
//! Collapsing them would either strip `location` from a mock or let a refusal set arbitrary headers.

use std::collections::BTreeSet;

use bytes::Bytes;
use pingora::http::StatusCode;

use super::header;

/// Headers only the refusal path may produce.
///
/// `www-authenticate` carries a challenge and `retry-after` a wait; both are consequences of a typed
/// [`Rejection`](crate::Rejection), so a plugin that wants one refuses instead of answering.
const REFUSAL_ONLY: [&str; 2] = ["www-authenticate", "retry-after"];

/// The framing header the writer recomputes, whatever the plugin wrote.
const CONTENT_LENGTH: &str = "content-length";

/// The connection header whose value nominates further headers to drop.
const CONNECTION: &str = "connection";

/// Why a synthesized response is not one the data plane can write.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ResponseError {
    /// The status is outside `100..=599`, so it is not an HTTP status code.
    #[error("response status must be in 100..=599, got {status}")]
    Status {
        /// The rejected status.
        status: u16,
    },

    /// A status that carries no body was given one.
    #[error("a {status} response cannot carry a body")]
    BodyNotAllowed {
        /// The status that cannot carry a body.
        status: u16,
    },

    /// A header name or value cannot be represented on the wire.
    #[error("response header `{name}` is invalid: {reason}")]
    InvalidHeader {
        /// The offending header name.
        name: String,
        /// Which rule it broke.
        reason: &'static str,
    },
}

/// A complete response produced by a plugin: a status, headers it would like to send, and a body.
///
/// Fields are private and the only constructor validates, so a value of this type is always one the
/// data plane *can* clean and write: this is where "a plugin cannot make the writer fail" is decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseData {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl ResponseData {
    /// Builds a response a plugin wants sent instead of forwarding the request.
    ///
    /// # Errors
    /// - [`ResponseError::Status`] when `status` is outside `100..=599`.
    /// - [`ResponseError::BodyNotAllowed`] when `status` is `1xx`, `204` or `304` and `body` is not
    ///   empty. A `HEAD` response is *not* rejected here: whether the body is written for a `HEAD`
    ///   request is the writer's decision, because this type does not see the method.
    /// - [`ResponseError::InvalidHeader`] when a name is not an RFC 9110 token or a value is not
    ///   visible ASCII. Hop-by-hop headers, `content-length`, `www-authenticate` and `retry-after`
    ///   are **accepted** and removed by the cleaning step (`clean_response`); refusing them here would make a plugin's
    ///   configuration depend on a rule the writer applies later.
    pub fn new(
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> Result<Self, ResponseError> {
        if !(100..=599).contains(&status) {
            return Err(ResponseError::Status { status });
        }
        if !carries_body(status) && !body.is_empty() {
            return Err(ResponseError::BodyNotAllowed { status });
        }
        for (name, value) in &headers {
            if !header::is_header_name(name) {
                return Err(ResponseError::InvalidHeader {
                    name: name.clone(),
                    reason: "not a header name token",
                });
            }
            if !header::is_header_value(value) {
                return Err(ResponseError::InvalidHeader {
                    name: name.clone(),
                    reason: "not a header value (visible ASCII, space or tab)",
                });
            }
        }
        Ok(Self {
            status,
            headers,
            body,
        })
    }

    /// Returns the status the plugin wants sent.
    #[must_use]
    pub const fn status(&self) -> u16 {
        self.status
    }

    /// Returns the headers the plugin produced, in the order it produced them.
    #[must_use]
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// Returns the body the plugin produced.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

/// A synthesized response after cleaning: what the data plane actually writes.
///
/// Produced only by [`clean_response`], so "the plugin's header list is not the wire head" holds for
/// every writer without each of them re-applying the rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CleanResponse {
    status: StatusCode,
    headers: Vec<(String, String)>,
    body: Bytes,
}

impl CleanResponse {
    /// Returns the status to write.
    pub(crate) const fn status(&self) -> StatusCode {
        self.status
    }

    /// Returns the end-to-end headers that survived cleaning, in their original order.
    pub(crate) fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// Returns the body to write, already emptied for a status that carries none.
    pub(crate) fn body(&self) -> &Bytes {
        &self.body
    }
}

/// Cleans a plugin's response into the bytes the one writer may send.
///
/// The rules, and why each exists:
///
/// - **Hop-by-hop headers are dropped**, including the ones a `connection` header nominates: a
///   synthesized response never travelled over a connection, and forwarding `transfer-encoding` would
///   invite a second framing decision on top of the writer's.
/// - **`content-length` is dropped.** It is recomputed from the body the writer actually sends; a
///   plugin-supplied value is exactly how a response body and its length desynchronize.
/// - **`www-authenticate` and `retry-after` are dropped.** They belong to a typed refusal, so a
///   plugin cannot claim one by returning a response.
/// - **A status that carries no body carries none here**, even though `ResponseData::new` already
///   refuses to build one: cleaning is the point at which the wire body is decided, so it stays total
///   on its own terms instead of trusting an earlier check.
/// - **Everything else is kept**, in order, including repeated `set-cookie` values and
///   `location`/`etag`, because a mock or a cache hit is supposed to look like a real answer.
pub(crate) fn clean_response(data: ResponseData) -> CleanResponse {
    let ResponseData {
        status,
        headers,
        body,
    } = data;
    let nominated = nominated_headers(&headers);
    let kept: Vec<(String, String)> = headers
        .into_iter()
        .filter(|(name, _)| is_forwardable(name, &nominated))
        .collect();
    // `ResponseData::new` accepted `status`, so `from_u16` cannot fail here: the constructor and the
    // parser both accept 100..=599. The fallback exists so this function stays total without an
    // `expect` on a non-test path.
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = if carries_body(status.as_u16()) {
        Bytes::from(body)
    } else {
        Bytes::new()
    };
    CleanResponse {
        status,
        headers: kept,
        body,
    }
}

/// Returns whether a status may carry a body.
fn carries_body(status: u16) -> bool {
    !(100..200).contains(&status) && status != 204 && status != 304
}

/// Collects the lowercase names the response's `connection` headers nominate.
fn nominated_headers(headers: &[(String, String)]) -> BTreeSet<String> {
    headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case(CONNECTION))
        .flat_map(|(_, value)| header::connection_tokens(value))
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Returns whether `name` may reach the wire.
fn is_forwardable(name: &str, nominated: &BTreeSet<String>) -> bool {
    let lowered = name.to_ascii_lowercase();
    !header::is_hop_by_hop(name)
        && lowered != CONTENT_LENGTH
        && !REFUSAL_ONLY.contains(&lowered.as_str())
        && !nominated.contains(&lowered)
}

#[cfg(test)]
mod tests;
