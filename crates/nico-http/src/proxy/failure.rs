//! The failure vocabulary in wire terms: one classification, one mapping, one writer.
//!
//! Pingora reports failures that this data plane never recorded (a connect refusal, a reset
//! downstream connection), and it calls [`fail_to_proxy`](super::HttpApp) exactly once per failed
//! request. This module turns either kind into one [`HttpError`] and writes the response that
//! [`HttpError::respond`] decided, so a status code and a body shape exist in exactly one place.

use std::{io, time::Duration};

use nico_core::ErrorCategory;
use pingora::{Error, ErrorSource, ErrorType, http::ResponseHeader, proxy::Session};

use crate::{
    context::HttpContext,
    error::{HttpError, Respond},
};

/// Records `error` and returns the Pingora error that aborts the hook.
///
/// The status is carried in the Pingora error as well, so even a framework path that bypassed
/// [`HttpApp::fail_to_proxy`] would answer with the status this crate decided.
pub(super) fn abort(ctx: &mut HttpContext, error: HttpError) -> Box<Error> {
    let wrapped = proxy_error(&error);
    ctx.fail(error);
    wrapped
}

/// Status reported to Pingora for a failure that has no response of its own.
///
/// Only `HttpError::ResponseCommitted` reaches it, and that variant never travels through `abort`;
/// it exists so this module has no unexplained numeric literal.
const FALLBACK_STATUS: u16 = 500;

/// Wraps our own failure as the Pingora error a hook must return.
pub(super) fn proxy_error(error: &HttpError) -> Box<Error> {
    // A failure with no renderable response (the response was already committed) has no status of
    // its own; Pingora still needs one for its own bookkeeping, and `500` is the honest answer.
    let status = error
        .respond()
        .map_or(FALLBACK_STATUS, |respond| respond.status().as_u16());
    let source = match error.category() {
        ErrorCategory::Client | ErrorCategory::Limit => ErrorSource::Downstream,
        ErrorCategory::Upstream => ErrorSource::Upstream,
        _ => ErrorSource::Internal,
    };
    Error::create(
        ErrorType::HTTPStatus(status),
        source,
        Some(error.to_string().into()),
        None,
    )
}

/// Derives this data plane's failure from one Pingora reported without being recorded first.
///
/// `None` means there is no response to write: a downstream read, write or close failure means the
/// client is gone, and answering it would be a status code nobody can receive.
///
/// A peer that ran out of the request's own budget is reported as the deadline failure, not as an
/// unreachable upstream. The two are different answers for the caller: `502` says "the peer is
/// broken", `504` says "the peer did not answer in time", and the whole-request deadline is the
/// budget this data plane set on the peer. Pingora reports that case as `ReadTimedout` (or a
/// `ConnectTimedout` before the socket ever opened), so the classification cannot be left to the
/// error source alone — an `Upstream` source is otherwise mapped to "unreachable".
pub(super) fn classify(ctx: &HttpContext, error: &Error) -> Option<HttpError> {
    if is_downstream_gone(error) {
        return None;
    }
    if is_upstream_timeout(error) {
        return Some(ctx.deadline_error());
    }
    let detail = error.to_string();
    match error.esource() {
        ErrorSource::Upstream => Some(HttpError::UpstreamFailed {
            source: Box::new(io::Error::other(detail)),
        }),
        ErrorSource::Downstream => Some(HttpError::InvalidRequest {
            reason: "the request could not be processed".into(),
        }),
        ErrorSource::Internal | ErrorSource::Unset => Some(HttpError::Internal {
            source: Box::new(io::Error::other(detail)),
        }),
    }
}

/// Reports whether a Pingora error means the upstream ran out of the request's budget.
pub(super) fn is_upstream_timeout(error: &Error) -> bool {
    if *error.esource() != ErrorSource::Upstream {
        return false;
    }
    matches!(
        error.etype(),
        ErrorType::ReadTimedout | ErrorType::WriteTimedout | ErrorType::ConnectTimedout
    )
}

/// Reports whether a Pingora error means the downstream connection ended.
pub(super) fn is_downstream_gone(error: &Error) -> bool {
    *error.esource() == ErrorSource::Downstream
        && matches!(
            error.etype(),
            ErrorType::ReadError
                | ErrorType::WriteError
                | ErrorType::ReadTimedout
                | ErrorType::WriteTimedout
                | ErrorType::ConnectionClosed
        )
}

/// Writes one mapped error response.
///
/// The header set is **closed**: `content-type`, `content-length`, `x-nico-error-code`,
/// `x-nico-trace-id`, plus `www-authenticate` for a rejection that carries a challenge and
/// `retry-after` for one that carries a wait. A policy supplies a template and a status, never a
/// header — otherwise one document could desynchronise `content-length` from the body it describes,
/// and nothing on the request path would notice.
///
/// `content-length` is taken from the rendered body, so a custom body and a custom content type
/// cannot disagree about how many bytes follow.
///
/// # Errors
/// Returns the failure of any step: building the header, or writing the header or the body. Every
/// one of them means the client is gone or the connection is unusable.
pub(super) async fn write_respond(
    session: &mut Session,
    respond: &Respond,
    trace_id: &str,
) -> pingora::Result<()> {
    let body = respond.body(trace_id);
    let mut header = ResponseHeader::build(respond.status(), None)?;
    header.set_content_length(body.len())?;
    header.insert_header("content-type", respond.content_type())?;
    // Both of these are stable identifiers: a custom body must not cost an operator the ability to
    // correlate a rejection with a log line.
    header.insert_header("x-nico-error-code", respond.code())?;
    header.insert_header("x-nico-trace-id", trace_id)?;
    if let Some(challenge) = respond.challenge() {
        header.insert_header("www-authenticate", challenge)?;
    }
    if let Some(retry_after) = respond.retry_after() {
        header.insert_header("retry-after", retry_after_seconds(retry_after).to_string())?;
    }
    // A header written with `end_of_stream: false` waits for the body that follows it.
    session
        .write_response_header(Box::new(header), false)
        .await?;
    session.write_response_body(Some(body), true).await
}

/// Rounds a wait up to whole seconds, the unit `retry-after` is written in.
///
/// Rounded **up**, never down: a caller that waits exactly what it was told must find a token
/// available, and truncating 900 ms to `0` would invite an immediate retry into the same rejection.
fn retry_after_seconds(wait: Duration) -> u64 {
    wait.as_secs() + u64::from(wait.subsec_nanos() > 0)
}

#[cfg(test)]
mod tests;
