//! The request-side metrics, emitted from the one place a request's facts are complete.
//!
//! This module is the **only** writer of the two `gateway_http_*` metrics. It lives beside the
//! request record because `record::emit` is the single point per request where the status, the
//! route id, the elapsed time and the error category are already decided — a second writer would
//! either double-count or miss the `fail_to_proxy` path.
//!
//! The recording call is unconditional: with no recorder installed the `metrics` facade drops it
//! after one atomic read, so a disabled metrics surface costs nothing on the request path and the
//! call site never becomes a second "is metrics enabled" decision.

use std::sync::Arc;

use nico_core::ErrorCategory;

use crate::{context::HttpContext, error::HttpError};

/// The `route_id` label value for a request that matched no route.
///
/// An explicit value rather than an empty string: a request that matched nothing and a route whose
/// id is the empty string are different facts, and an empty label would fold them together.
const UNMATCHED: &str = "unmatched";

/// Records the two request metrics once, from the facts [`emit`](super::emit) already computed.
pub(super) fn record_request(status: u16, ctx: &HttpContext, elapsed_ms: u64) {
    // The route id is shared with the context, so the label clones one `Arc` instead of copying the
    // bytes. `status_class` and `error_label` are `'static`, and the two calls are independent.
    //
    // `route_id` is cloned once below because each `metrics` macro consumes its label values; the
    // clone is an `Arc` refcount bump, not a string copy, which is why it is not worth restructuring
    // to share one value across the two calls.
    let route_id = match ctx.route_id_handle() {
        Some(id) => metrics::SharedString::from(Arc::clone(id)),
        None => metrics::SharedString::const_str(UNMATCHED),
    };
    metrics::counter!(
        "gateway_http_requests_total",
        "route_id" => route_id.clone(),
        "status_class" => status_class(status),
        "error_category" => error_label(ctx.error().map(HttpError::category)),
    )
    .increment(1);
    metrics::histogram!(
        "gateway_http_request_seconds",
        "route_id" => route_id,
    )
    .record(elapsed_ms as f64 / 1000.0);
}

/// Classifies a status into the `status_class` label value.
///
/// The set is the whole cardinality budget for this label: the log-only `499` belongs to `4xx`,
/// and `0` — nothing written and nothing cancelled — is `none` rather than a class of its own.
fn status_class(status: u16) -> &'static str {
    match status {
        100..=199 => "1xx",
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        500..=599 => "5xx",
        _ => "none",
    }
}

/// The `error_category` label value for a failure, or `none` when the request had none.
///
/// The spelling comes from [`ErrorCategory::as_str`] rather than from a match here: the label and
/// the category's serde form are the same string, and a second copy would let the two drift until a
/// dashboard and a golden fixture disagreed about what a category is called. `none` is this label's
/// own value — "no error at all" is a fact about the request, not a category of failure.
fn error_label(error: Option<ErrorCategory>) -> &'static str {
    match error {
        Some(category) => category.as_str(),
        None => "none",
    }
}
