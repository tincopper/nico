//! The LLM route's share of the hooks: one body buffered, one head replaced, one body translated.
//!
//! Every function here takes the framework header it needs instead of a `Session`, so the translation
//! can be asserted without a connection. The order of the operations inside them is the contract:
//!
//! - **Upstream request head**: the provider's path and headers are written *before* `Host` and
//!   `X-Forwarded-*`, so a route can never choose the peer it is written to. A converting route also
//!   takes the content coding away from the caller: it re-frames the answer itself, so a coding it
//!   will not decode becomes a parse failure behind a status line that is already committed.
//! - **Request body**: `convert` accumulates to end of stream and calls
//!   [`ConvertingRoute::prepare`](crate::llm::ConvertingRoute::prepare) exactly once; `passthrough` touches nothing.
//! - **Response head**: the translation runs before anything is written, so a declared length over the
//!   route's ceiling can still be answered `502`. A head that cannot carry a body is passed through
//!   instead: there is nothing to decode, and a client-protocol envelope cannot be invented. An answer
//!   that arrives before the client's body ends — a provider refusing a credential it never had to read
//!   a body for — is translated as a buffered one, because no event stream can begin before its
//!   request finishes.
//! - **Response body**: an installed transform sees each provider chunk and the end of the body; the
//!   bytes it returns are the ones written. No plugin body or response stage runs on such a route.
//!
//! Nothing here logs a header value or a body byte: a credential arrives in one and a prompt in the
//! other.

use bytes::Bytes;
use pingora::http::{Method, RequestHeader, ResponseHeader};

use super::response::may_carry_body;
use crate::{
    budget,
    context::{FailureStage, HttpContext},
    error::HttpError,
    llm::{LlmRequest, LlmResponseHead, is_content_coding, is_data_plane_owned},
};

/// Writes the provider's path and headers over the client's, and drops what the data plane now owns.
///
/// `convert` replaces the body, so the caller's `authorization` describes nothing the provider will
/// see: it is removed even when the route presents no credential of its own, because sending a
/// caller's key to a peer that did not issue it is a leak, not a default. The content codings go with
/// it: the answer's head is written from the front-end's own headers with the provider's removed, so
/// a coding negotiated here would describe bytes this crate never decodes.
///
/// `passthrough` keeps all three: its bytes are the caller's, so the coding and the credential that
/// describe them travel unchanged — unless the route presents a credential of its own, which replaces
/// the caller's, because a route that materializes a key is the gateway authenticating to that peer.
pub(in crate::proxy) fn apply_upstream_request(
    request: &mut RequestHeader,
    ctx: &HttpContext,
) -> Result<(), HttpError> {
    let Some(llm) = ctx.llm() else {
        return Ok(());
    };
    // The attempt's own facts: a fallback target has its own path and headers, and the head written
    // for it must be that target's. The route's values are only the fallback for a passthrough route,
    // which has no walk to ask.
    let (path, headers) = match llm.target() {
        Some(target) => (target.path(), target.headers()),
        None => (llm.route().path(), llm.route().headers()),
    };
    request.set_raw_path(path.as_bytes()).map_err(|error| {
        HttpError::internal(format!(
            "the provider path is not a request target: {error}"
        ))
    })?;
    if llm.converting().is_some() {
        // The caller's credential describes the caller–gateway relationship, and a converted request
        // replaces that relationship with the route's own. Every client protocol this gateway serves
        // carries it in a different spelling (`authorization`, `x-api-key`, `x-goog-api-key`), so all
        // of them go: sending a caller's key to a peer that did not issue it is a leak, not a default.
        request.remove_header("authorization");
        request.remove_header("x-api-key");
        request.remove_header("x-goog-api-key");
        request.remove_header("accept-encoding");
        // A converted body is the exchange's to decode, and a content-coded one is not a body any
        // codec here reads. Refusing it names the reason instead of letting the codec report the
        // compressed bytes as malformed JSON.
        if let Some(coding) = request.headers.get("content-encoding")
            && !coding.as_bytes().eq_ignore_ascii_case(b"identity")
        {
            return Err(HttpError::InvalidRequest {
                reason:
                    "the request body is content-encoded, which a converted route cannot decode"
                        .into(),
            });
        }
    }
    for (name, value) in headers {
        // The data plane frames the message, chooses the peer and negotiates the coding: a route
        // that names one of those headers does not get to overrule it.
        if is_data_plane_owned(name) || is_content_coding(name) {
            continue;
        }
        request
            .insert_header(name.to_owned(), value.to_owned())
            .map_err(|error| {
                HttpError::internal(format!("a provider header could not be written: {error}"))
            })?;
    }
    Ok(())
}

/// Accumulates a converted request body and prepares it once the client has finished sending.
///
/// Returns whether this route owned the body, so the caller stops before offering the bytes to a
/// plugin: `convert` accumulates and prepares, `passthrough` forwards them untouched, and a route that
/// is not an LLM route answers `false` without touching either.
///
/// Holding a chunk back is `Some(Bytes::new())` with `end_of_stream == false`: the framework skips an
/// empty chunk that is not the end, so nothing reaches the provider until the prepared body does.
/// `None` with the end flag is a request that had no body at all, or one whose prepared body is empty —
/// the front-end still sees both, because "the client sent nothing" is a request the codec must reject
/// rather than one to forward, and an empty product is not a chunk to write.
pub(in crate::proxy) fn request_body(
    ctx: &mut HttpContext,
    request: &RequestHeader,
    body: &mut Option<Bytes>,
    end_of_stream: bool,
) -> Result<bool, HttpError> {
    if ctx.converting().is_none() {
        // A route that is not converting owns the body only when it is an LLM route: a passthrough
        // route forwards the bytes by not touching them, and any other route leaves them to the
        // plugin stages.
        return Ok(ctx.llm().is_some());
    }

    // A retry round replays Pingora's retry buffer through this hook, so the client's body is
    // offered a second time. The walk already decoded it once, and its canonical value is what the
    // next attempt re-encodes: accumulating the replay again would decode it twice (the design's one
    // invariant). The bytes to send are the ones the walk encoded for this attempt — written once,
    // here, at the round's end of stream.
    if ctx.llm().is_some_and(|llm| llm.body_already_prepared()) {
        if !end_of_stream {
            // Hold the replay back: the framework skips an empty non-final chunk, so nothing reaches
            // the provider until the whole body has been replayed and this round writes its bytes.
            *body = Some(Bytes::new());
            return Ok(true);
        }
        // A round that moved to another target has not encoded anything yet — its plan is built
        // here, from the canonical value the first round decoded. A budget retry stayed on its
        // member and already encoded, so its bytes are simply read back.
        let prepared = {
            let Some(attempt) = ctx.llm_mut().and_then(|llm| llm.attempt_mut()) else {
                return Ok(true);
            };
            if attempt.body().is_none() {
                // Re-encode for the member this round addresses. `decode` is skipped: the walk
                // remembers the canonical value, and the invariant is one decode per request.
                attempt.prepare(
                    &LlmRequest::new(
                        request.method.as_str(),
                        request
                            .uri
                            .path_and_query()
                            .map_or_else(|| request.uri.path(), |target| target.as_str()),
                        utf8_headers(request),
                        &[],
                    ),
                    tokio::time::Instant::now(),
                )?;
            }
            attempt.body().map(<[u8]>::to_vec).unwrap_or_default()
        };
        *body = (!prepared.is_empty()).then(|| Bytes::from(prepared));
        return Ok(true);
    }

    match body.as_ref() {
        Some(chunk) => ctx.push_body(chunk),
        None if !end_of_stream => return Ok(true),
        None => {}
    }
    if !end_of_stream {
        *body = Some(Bytes::new());
        return Ok(true);
    }

    let whole = ctx.take_body();
    let prepared = {
        let view = LlmRequest::new(
            request.method.as_str(),
            // The facts carry the target with its query: a protocol may name a request fact there
            // (the `alt` spelling, for one), and the contract documents the path as including it.
            // A target with no path-and-query part (an authority form) has no query to carry.
            request
                .uri
                .path_and_query()
                .map_or_else(|| request.uri.path(), |target| target.as_str()),
            utf8_headers(request),
            &whole,
        );
        let Some(attempt) = ctx.llm_mut().and_then(|llm| llm.attempt_mut()) else {
            return Err(HttpError::internal(
                "the request body was buffered without a converting route",
            ));
        };
        attempt.prepare(&view, tokio::time::Instant::now())?
    };
    if let Some(llm) = ctx.llm_mut() {
        llm.record_prepared(&prepared);
        // The replay of this body — if the attempt fails and another round runs — must not be
        // accumulated again; the canonical value it produced is what the next attempt re-encodes.
        llm.mark_body_prepared();
    }
    // An empty final body is `None`, not an empty chunk: the framework writes an empty chunk as a
    // terminator and then terminates again, which is two terminators on the wire.
    let prepared_body = prepared.into_body();
    *body = (!prepared_body.is_empty()).then(|| Bytes::from(prepared_body));
    Ok(true)
}

/// Installs the response transform and writes the head the client protocol answers with.
///
/// A `passthrough` route installs nothing: its bytes are the client's protocol already. A head that
/// cannot carry a body is left alone for the same reason the body hook is never called for it.
///
/// The transform is created here, before the head is written, so a front-end that cannot serve the
/// response is answered with its own status instead of half a body.
pub(in crate::proxy) fn install_transform(
    ctx: &mut HttpContext,
    request: &RequestHeader,
    response: &mut ResponseHeader,
) -> Result<(), HttpError> {
    if ctx.converting().is_none() {
        return Ok(());
    }
    // The response head is where an attempt's fate is decided: a 2xx is the answer and the transform
    // translates it; anything else is the walk's to classify, and it may answer "retry" — in which
    // case nothing is written and the attempt loop sends the next target.
    classify_head(ctx, response.status.as_u16())?;
    if !may_carry_body(request.method == Method::HEAD, response) {
        return Ok(());
    }
    let limit = ctx
        .response_limit()
        .ok_or_else(|| HttpError::internal("no response limit was bound to the request"))?;
    let mut transform = {
        let Some(llm) = ctx.llm() else {
            return Ok(());
        };
        // A provider may answer before the client's body has ended: an invalid credential is refused
        // from the request head alone, and this hook runs while the upload is still in flight. No
        // event stream can begin before its request ends, so such an answer is not one — the
        // transform is created buffered, which is how a refusal is rendered whole. Reporting the
        // missing flag as a defect would answer `500` for the provider's own `401`.
        let streaming = llm.streaming().unwrap_or(false);
        // Only a buffered answer is bounded by this ceiling: an event stream may legitimately declare
        // its total length and is still translated chunk by chunk.
        if !streaming
            && let Some(declared) = budget::declared_response_length(response)
            && budget::over_limit(limit, declared)
        {
            return Err(HttpError::UpstreamResponseTooLarge { limit });
        }
        let Some(converting) = ctx.converting() else {
            return Ok(());
        };
        converting.transform(streaming, limit)?
    };

    let head = transform.head(response.status.as_u16(), &utf8_response_headers(response))?;
    apply_head(response, &head)?;
    response
        .insert_header("x-nico-trace-id", ctx.trace_id())
        .map_err(|error| {
            HttpError::internal(format!("the trace id could not be written: {error}"))
        })?;
    if let Some(llm) = ctx.llm_mut() {
        llm.install(transform);
    }
    Ok(())
}

/// Asks the walk what a response head means, and turns "retry" into the failure that ends the
/// attempt.
///
/// A 2xx is recorded as the target's success and answered as-is. A non-2xx is the walk's to classify:
/// `Answer`/`Exhausted` keep the response for the data plane to translate, while `Retry` returns the
/// error that aborts this attempt — the loop's next round then writes the next target's head, and
/// Pingora's own retry accounting is what drives it.
fn classify_head(ctx: &mut HttpContext, status: u16) -> Result<(), HttpError> {
    let now = tokio::time::Instant::now();
    let Some(attempt) = ctx.llm_mut().and_then(|llm| llm.attempt_mut()) else {
        return Ok(());
    };
    if (200..300).contains(&status) {
        attempt.succeeded(now);
        return Ok(());
    }
    match attempt.classify(status, now)? {
        crate::llm::AttemptVerdict::Retry => {
            // The walk holds the next attempt already. Ending this attempt here is what makes the
            // framework take the next round: no byte reached the client, so the request is still
            // replayable, and `error_while_proxy` decides whether the retry may proceed. The error
            // is the walk's own signal, not a client-visible failure: `fail_to_proxy` never writes
            // it, because the next round replaces this attempt entirely.
            log_replaced_attempt(ctx);
            Err(HttpError::AttemptRetry)
        }
        crate::llm::AttemptVerdict::Answer | crate::llm::AttemptVerdict::Exhausted => Ok(()),
    }
}

/// Decides whether the round that just failed is replaced by another attempt.
///
/// The one retry decision, so that no hook can forget a fact the design requires. Two halves, in this
/// order because the first is free and cannot move anything:
///
/// 1. **the data plane's facts** — the client is there, nothing was committed, and the body can be
///    produced again for the stage the failure came from
///    ([`HttpContext::may_replace_attempt`]);
/// 2. **the walk's answer** — a round that produced `AttemptRetry` already moved, and any other
///    failure is classified here, once, with the clock: the walk is what counts a failed target
///    against its breaker, so a failure no hook recorded still reaches it. The mechanical facts come
///    first, which is why an unreplayable body ends the request without spending an attempt it
///    cannot use.
///
/// `true` means the round is replaced: its failure is cleared — a retried attempt is not the request's
/// outcome — and the failure line is written, because the client only ever sees the final answer.
/// `false` leaves the request able to answer with whatever failure it holds.
pub(in crate::proxy) fn retry_admitted(
    ctx: &mut HttpContext,
    committed: bool,
    stage: FailureStage,
) -> bool {
    if !ctx.may_replace_attempt(committed, stage) {
        return false;
    }
    if !matches!(ctx.error(), Some(HttpError::AttemptRetry)) && !classify_failed(ctx) {
        return false;
    }
    ctx.clear_failure();
    log_replaced_attempt(ctx);
    true
}

/// Asks the walk to classify one failed attempt, and reports whether another is ready.
///
/// Used for a failure no hook recorded: the framework's own upstream interaction. A failure the hooks
/// did record was classified where it happened, because the classification is what decided the
/// response.
///
/// The two walks answer with their own vocabularies: an llm route's attempt speaks in canonical
/// refusals and chain positions, and a traffic route's member walk only says whether the budget
/// holds another member. Exactly one walk can exist for a request.
fn classify_failed(ctx: &mut HttpContext) -> bool {
    let verdict = ctx
        .llm_mut()
        .and_then(|llm| llm.attempt_mut())
        .map(|attempt| attempt.failed(tokio::time::Instant::now()))
        .transpose();
    match verdict {
        Ok(Some(crate::llm::AttemptVerdict::Retry)) => return true,
        // The llm walk refused or exhausted: no llm attempt follows, and a traffic walk cannot exist
        // beside one.
        Ok(None) => {}
        _ => return false,
    }
    matches!(ctx.traffic_walk_mut().map(|walk| walk.failed()), Some(true))
}

/// Reports one attempt that failed, whose outcome the walk has already replaced.
///
/// One `warn!` per replaced attempt: without it a retry that succeeds would leave the first failure
/// invisible, because the client only ever sees the final answer. The attempt that *produced* the
/// client's answer takes no line — its outcome is the request record's, and a successful first
/// attempt therefore stays silent, as a per-request log must.
pub(in crate::proxy) fn log_replaced_attempt(ctx: &mut HttpContext) {
    let Some(llm) = ctx.llm_mut() else {
        return;
    };
    let Some(attempt) = llm.take_attempt_in_flight() else {
        return;
    };
    tracing::warn!(
        llm.target = %attempt.target,
        llm.attempt = attempt.number,
        llm.outcome = "failed",
        duration_ms = u64::try_from(attempt.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "LLM attempt failed"
    );
}

/// Translates one response-body task through the installed transform.
///
/// A transform may answer nothing for a chunk (the event spans chunks) or nothing at the end (the
/// body is complete); either way the framework writes no bytes for it rather than treating the empty
/// chunk as the end of the message.
pub(in crate::proxy) fn transform_response_body(
    ctx: &mut HttpContext,
    body: &mut Option<Bytes>,
    end_of_stream: bool,
) -> Result<(), HttpError> {
    // The framework also calls this hook with a zero-byte buffer while a chunked body's terminator or
    // trailers are still being parsed. Nothing arrived, so nothing is translated: handing the empty
    // chunk on would make every front-end responsible for tolerating it.
    if body.as_ref().is_some_and(|chunk| chunk.is_empty()) && !end_of_stream {
        return Ok(());
    }
    let produced = {
        let Some(transform) = ctx.llm_mut().and_then(|llm| llm.transform_mut()) else {
            return Ok(());
        };
        if body.is_none() && !end_of_stream {
            return Ok(());
        }
        let mut produced = match body.as_ref() {
            Some(chunk) => transform.push(chunk)?,
            None => Vec::new(),
        };
        if end_of_stream {
            produced.extend_from_slice(&transform.finish()?);
        }
        produced
    };
    *body = (!produced.is_empty()).then(|| Bytes::from(produced));
    Ok(())
}

/// Replaces the provider's head with the one the client protocol answers with.
///
/// Every provider header is removed, not merely overwritten: a header the translation did not produce
/// describes the provider's message, and forwarding it would describe a body the caller never
/// receives. The framing headers go with the rest — the data plane frames what it writes.
fn apply_head(response: &mut ResponseHeader, head: &LlmResponseHead) -> Result<(), HttpError> {
    response
        .set_status(head.status())
        .map_err(|error| HttpError::internal(format!("the client status is not valid: {error}")))?;
    let names: Vec<String> = response
        .headers
        .keys()
        .map(|name| name.as_str().to_owned())
        .collect();
    for name in &names {
        response.remove_header(name.as_str());
    }
    for (name, value) in head.headers() {
        response
            .append_header(name.to_owned(), value.to_owned())
            .map_err(|error| {
                HttpError::internal(format!("a client header could not be written: {error}"))
            })?;
    }
    Ok(())
}

/// Collects the request headers whose value is UTF-8, in the order the client sent them.
///
/// A value that is not UTF-8 is omitted: a codec reads text, and a mangled copy of a credential is
/// worse than an absent header. The body is never omitted.
fn utf8_headers(request: &RequestHeader) -> Vec<(&str, &str)> {
    request
        .headers
        .iter()
        .filter_map(|(name, value)| Some((name.as_str(), value.to_str().ok()?)))
        .collect()
}

/// Collects the provider response headers whose value is UTF-8, in arrival order.
fn utf8_response_headers(response: &ResponseHeader) -> Vec<(String, String)> {
    response
        .headers
        .iter()
        .filter_map(|(name, value)| {
            Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
        })
        .collect()
}
