//! The resource budget of one request: body bytes and time.
//!
//! A route may tighten a document-wide limit but never loosen it, so both budgets are the smaller
//! of the two values. The rule lives here once: the header pre-check, the streaming body counter and
//! the deadline all call these functions instead of repeating the comparison, which is what keeps a
//! route override from silently widening a document bound.

use std::time::Duration;

use nico_core::{RequestBounds, Route};
use pingora::http::{HMap, RequestHeader, ResponseHeader};

/// Returns the request body limit: the smaller of the route override and the document limit.
#[must_use]
pub(crate) fn body_limit(route: &Route, request: RequestBounds) -> u32 {
    let document_limit = request.max_body_bytes();
    route
        .max_body_bytes()
        .map_or(document_limit, |route_limit| {
            route_limit.min(document_limit)
        })
}

/// Returns the ceiling on a buffered response body: the smaller of the route override and the
/// document limit.
///
/// The same "inner may tighten, never widen" rule as the two request limits, applied to the third
/// bound. It is read by the response-head hook, which refuses a response that declares more than this
/// before the head is written, and passed to the response transform, which holds a chunked answer to
/// the same number while it buffers it.
#[must_use]
pub(crate) fn response_limit(route: &Route, request: RequestBounds) -> u32 {
    let document_limit = request.max_response_bytes();
    route
        .max_response_bytes()
        .map_or(document_limit, |route_limit| {
            route_limit.min(document_limit)
        })
}

/// Returns the body length the request declares in `Content-Length`, when it declares one.
///
/// A request without the header — chunked framing, or HTTP/2 without a declared length — streams
/// and is metered by [`over_limit`] instead. A value that does not parse reads as no declaration:
/// Pingora rejects a malformed length before this data plane sees the header, and guessing at a
/// number it refused to validate would be worse than metering what actually arrives.
#[must_use]
pub(crate) fn declared_length(header: &RequestHeader) -> Option<u64> {
    declared_length_in(&header.headers)
}

/// Returns the body length the response declares in `Content-Length`, when it declares one.
///
/// The response-side counterpart of [`declared_length`], used by the one check that can still answer
/// `502`: a non-streaming answer whose declared size is over the route's ceiling is refused before its
/// head is written. A chunked answer declares nothing and is metered by the transform instead.
#[must_use]
pub(crate) fn declared_response_length(header: &ResponseHeader) -> Option<u64> {
    declared_length_in(&header.headers)
}

/// Reads `Content-Length` out of a header map, refusing anything that is not a plain number.
fn declared_length_in(headers: &HMap) -> Option<u64> {
    headers
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
}

/// Reports whether a body that has delivered `received` bytes has crossed `limit`.
///
/// A body exactly at the limit is accepted: the limit is inclusive.
#[must_use]
pub(crate) const fn over_limit(limit: u32, received: u64) -> bool {
    received > limit as u64
}

/// Returns the whole-request budget: the smaller of the route override and the document limit.
#[must_use]
pub(crate) fn request_budget(route: &Route, request: RequestBounds) -> Duration {
    let document_budget = request.timeout_ms();
    let budget_ms = route.timeout_ms().map_or(document_budget, |route_budget| {
        route_budget.min(document_budget)
    });
    Duration::from_millis(u64::from(budget_ms))
}

/// Returns the document-wide budget, before any route has matched.
///
/// A global plugin runs before routing and still runs inside the request's remaining time, so the
/// budget has to exist before a route does. [`request_budget`] then tightens it; a route can never
/// widen what this returned.
#[must_use]
pub(crate) fn document_budget(request: RequestBounds) -> Duration {
    Duration::from_millis(u64::from(request.timeout_ms()))
}

#[cfg(test)]
mod tests;
