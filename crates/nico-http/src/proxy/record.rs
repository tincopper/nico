//! The request's one structured log record.
//!
//! This lives next to the hooks instead of inside them because "what an operator reads for one
//! request" is a decision of its own: the hook contract decides *when* the record is emitted, this
//! module decides *what it says and at which level*. Keeping them apart is also what keeps
//! `hooks.rs` inside the file budget.
//!
//! There is no unit test here on purpose: the subject is the text an operator reads, and the
//! end-to-end targets assert exactly that (`http_cancel.rs` for `499` + `Cancelled`,
//! `http_limits.rs` for `408` + `Client`, `http_routing.rs` for a rejected route, and
//! `llm_convert.rs`/`llm_passthrough.rs` for the `llm.*` fields).

use std::time::Instant;

use nico_core::ErrorCategory;
use pingora::{Error, http::Method, proxy::Session};
use tracing::Level;

use super::failure::is_downstream_gone;
use crate::context::HttpContext;

mod metrics;

/// The log-only status for a request that was cancelled before anything was written.
///
/// `499` is nginx's convention and the value the backend error spec assigns to `Cancelled`; it is
/// never sent on the wire, only reported.
const CANCELLED_STATUS: u16 = 499;

/// Emits one record, adding `authn`, `plugin` and the LLM facts only when those facts exist.
///
/// `tracing` cannot add a field conditionally inside one invocation, and writing `authn = ""` would
/// claim "authenticated by nothing" where the truth is "not authenticated" — two different facts for
/// anyone filtering the log. `plugin` is the instance a failure is attributed to, absent when no
/// plugin failed, so a 500 caused by a policy can be told apart from one caused by an upstream.
///
/// An LLM route reports its two publication facts (`llm.protocol`, `llm.provider`), and — once its
/// client body has been decoded — the model pair and the stream flag (`llm.model`, `llm.stream`).
/// A route that is not an LLM route reports none of them, and a decoded request always reports all
/// four: an empty `llm.protocol` would claim a protocol whose name is the empty string, and
/// `llm.stream = false` would claim a caller asked for a complete answer when it asked for nothing
/// this route decoded. The level stays a literal at each call site, so every expansion still has
/// static callsite metadata.
macro_rules! record {
    // A route that is not an LLM route: none of the `llm.*` fields exists.
    ($level:expr, $ctx:expr, $message:literal, $($fields:tt)*) => {
        match ($ctx.identity(), $ctx.failed_plugin()) {
            (Some(identity), Some(plugin)) => tracing::event!(
                $level, authn = %identity.source(), plugin = %plugin, $($fields)* $message
            ),
            (Some(identity), None) => tracing::event!(
                $level, authn = %identity.source(), $($fields)* $message
            ),
            (None, Some(plugin)) => tracing::event!(
                $level, plugin = %plugin, $($fields)* $message
            ),
            (None, None) => tracing::event!($level, $($fields)* $message),
        }
    };

    // An LLM route whose client body was decoded: the model pair and the stream flag as well.
    ($level:expr, $ctx:expr, llm = $llm:ident, prepared, $message:literal, $($fields:tt)*) => {
        match ($ctx.identity(), $ctx.failed_plugin()) {
            (Some(identity), Some(plugin)) => tracing::event!(
                $level,
                authn = %identity.source(),
                plugin = %plugin,
                llm.protocol = %$llm.protocol(),
                llm.provider = %$llm.provider(),
                llm.model = ?$llm.model().unwrap_or_default(),
                llm.stream = $llm.streaming().unwrap_or_default(),
                $($fields)* $message
            ),
            (Some(identity), None) => tracing::event!(
                $level,
                authn = %identity.source(),
                llm.protocol = %$llm.protocol(),
                llm.provider = %$llm.provider(),
                llm.model = ?$llm.model().unwrap_or_default(),
                llm.stream = $llm.streaming().unwrap_or_default(),
                $($fields)* $message
            ),
            (None, Some(plugin)) => tracing::event!(
                $level,
                plugin = %plugin,
                llm.protocol = %$llm.protocol(),
                llm.provider = %$llm.provider(),
                llm.model = ?$llm.model().unwrap_or_default(),
                llm.stream = $llm.streaming().unwrap_or_default(),
                $($fields)* $message
            ),
            (None, None) => tracing::event!(
                $level,
                llm.protocol = %$llm.protocol(),
                llm.provider = %$llm.provider(),
                llm.model = ?$llm.model().unwrap_or_default(),
                llm.stream = $llm.streaming().unwrap_or_default(),
                $($fields)* $message
            ),
        }
    };

    // An LLM route whose body was never decoded: a passthrough route, or a provider that answered
    // before the client finished sending. Only the two publication facts are known.
    ($level:expr, $ctx:expr, llm = $llm:ident, $message:literal, $($fields:tt)*) => {
        match ($ctx.identity(), $ctx.failed_plugin()) {
            (Some(identity), Some(plugin)) => tracing::event!(
                $level,
                authn = %identity.source(),
                plugin = %plugin,
                llm.protocol = %$llm.protocol(),
                llm.provider = %$llm.provider(),
                $($fields)* $message
            ),
            (Some(identity), None) => tracing::event!(
                $level,
                authn = %identity.source(),
                llm.protocol = %$llm.protocol(),
                llm.provider = %$llm.provider(),
                $($fields)* $message
            ),
            (None, Some(plugin)) => tracing::event!(
                $level,
                plugin = %plugin,
                llm.protocol = %$llm.protocol(),
                llm.provider = %$llm.provider(),
                $($fields)* $message
            ),
            (None, None) => tracing::event!(
                $level,
                llm.protocol = %$llm.protocol(),
                llm.provider = %$llm.provider(),
                $($fields)* $message
            ),
        }
    };
}

/// Chooses the record shape the request's facts allow.
///
/// Two independent facts decide the shape, and both are answered by absence rather than by an empty
/// value:
///
/// - `upstream.endpoint` exists only for a request that selected one member of a multi-member
///   upstream. A single-member deployment names its only machine in `upstream` already, and writing
///   the member there would change the shape of every existing deployment's logs to say nothing new.
/// - the `llm.*` fields exist only on an AI route: [`LlmExchange::record_prepared`] sets the model
///   label and the stream flag together, and a passthrough route never calls it. A front-end that
///   reports a stream flag without a model label is recorded with the two publication facts only.
macro_rules! emit {
    ($level:expr, $ctx:expr, $message:literal, $($fields:tt)*) => {
        match $ctx.upstream_endpoint() {
            Some(endpoint) => {
                shape!($level, $ctx, $message, upstream.endpoint = %endpoint, $($fields)*)
            }
            None => shape!($level, $ctx, $message, $($fields)*),
        }
    };
}

/// Chooses the `llm.*` shape, having already decided the member field.
macro_rules! shape {
    ($level:expr, $ctx:expr, $message:literal, $($fields:tt)*) => {
        match $ctx.llm() {
            Some(llm) if llm.model().is_some() => {
                record!($level, $ctx, llm = llm, prepared, $message, $($fields)*)
            }
            Some(llm) => record!($level, $ctx, llm = llm, $message, $($fields)*),
            None => record!($level, $ctx, $message, $($fields)*),
        }
    };
}

/// Emits the request's one record, at the level the operator's action implies.
///
/// The level is that action: a normal request is `debug!` (the spec forbids `info!` on the request
/// path), a rejection the caller can fix is `warn!`, and a failure the operator must act on is
/// `error!`. No body, credential or query string is logged.
///
/// The charge of a shared budget store is awaited here: the logging hook is async, so the
/// request's own accounting completes before the record is emitted — and a charge that cannot be
/// confirmed is logged as the failure it is, never silently dropped.
pub(super) async fn emit(session: &Session, error: Option<&Error>, ctx: &HttpContext) {
    let written = session
        .response_written()
        .map(|response| response.status.as_u16());
    // A request nobody answered is logged as `499`, the log-only status the error spec reserves for
    // cancellation; a request that had already written a response keeps its real status. `0` stays
    // for "nothing was written and nothing was cancelled" and for an unpublished snapshot
    // (`Generation::ZERO`), which is exactly what happened.
    let cancelled =
        ctx.is_cancelled() || (ctx.error().is_none() && error.is_some_and(is_downstream_gone));
    let status = written.unwrap_or(if cancelled { CANCELLED_STATUS } else { 0 });
    let elapsed_ms = ctx.elapsed_ms(Instant::now());
    let route_id = ctx.route_id().unwrap_or_default();
    let upstream = ctx.upstream().unwrap_or_default();
    let generation = ctx.generation().map_or(0, nico_core::Generation::get);
    let trace_id = ctx.trace_id();
    let method = ctx.method().map_or("", Method::as_str);
    let body_bytes = ctx.body_bytes();

    // The metrics are recorded once, here, from the same facts the log record below uses. The
    // call is unconditional: with no recorder installed the facade is a no-op.
    metrics::record_request(status, ctx, elapsed_ms);

    // A cancellation is checked first: a request whose client left, or whose deadline expired after
    // the status line was already sent, has a recorded cause but no response to report, so it
    // belongs at `debug!` rather than in the error stream.
    if cancelled {
        match ctx.error() {
            Some(failure) => emit!(
                Level::DEBUG,
                ctx,
                "request cancelled",
                trace_id = %trace_id,
                route_id = %route_id,
                upstream = %upstream,
                config.generation = generation,
                http.method = method,
                http.status = status,
                body_bytes = body_bytes,
                elapsed_ms = elapsed_ms,
                error.category = ?ErrorCategory::Cancelled,
                error = %failure,
            ),
            None => emit!(
                Level::DEBUG,
                ctx,
                "request cancelled",
                trace_id = %trace_id,
                route_id = %route_id,
                upstream = %upstream,
                config.generation = generation,
                http.method = method,
                http.status = status,
                body_bytes = body_bytes,
                elapsed_ms = elapsed_ms,
                error.category = ?ErrorCategory::Cancelled,
            ),
        }
        return;
    }

    if let Some(failure) = ctx.error() {
        // A rejection the caller can fix is `warn!`; everything else is a failure the operator must
        // act on. Neither `info!` nor a per-request level above these is allowed on the request
        // path.
        match failure.category() {
            ErrorCategory::Client | ErrorCategory::Limit => emit!(
                Level::WARN,
                ctx,
                "request rejected",
                trace_id = %trace_id,
                route_id = %route_id,
                upstream = %upstream,
                config.generation = generation,
                http.method = method,
                http.status = status,
                body_bytes = body_bytes,
                elapsed_ms = elapsed_ms,
                error.category = ?failure.category(),
                error = %failure,
            ),
            _ => emit!(
                Level::ERROR,
                ctx,
                "request failed",
                trace_id = %trace_id,
                route_id = %route_id,
                upstream = %upstream,
                config.generation = generation,
                http.method = method,
                http.status = status,
                body_bytes = body_bytes,
                elapsed_ms = elapsed_ms,
                error.category = ?failure.category(),
                error = %failure,
            ),
        }
        return;
    }

    emit!(
        Level::DEBUG,
        ctx,
        "request completed",
        trace_id = %trace_id,
        route_id = %route_id,
        upstream = %upstream,
        config.generation = generation,
        http.method = method,
        http.status = status,
        body_bytes = body_bytes,
        elapsed_ms = elapsed_ms,
    );

    record_tokens(ctx, status).await;
}

/// Emits the token usage a successful converted exchange reported.
///
/// Nothing here is a gateway measurement: these are the provider's own numbers, the ones a bill is
/// made of. A request that reported none produces no sample and no field — "no number" and "zero" are
/// different facts, and a consumer averaging over them would be computing something nobody asked for.
///
/// The suppression rules, each with its own reason:
///
/// - **not 2xx**: a failure has no usage, and a 4xx never had one;
/// - **no transform**: a passthrough route has no canonical value, so nothing to report;
/// - **no selected target**: the request never reached a provider.
async fn record_tokens(ctx: &crate::HttpContext, status: u16) {
    if !(200..300).contains(&status) {
        return;
    }
    let Some(llm) = ctx.llm() else {
        return;
    };
    let Some(usage) = llm.transform_usage() else {
        return;
    };
    let route_id = ctx.route_id().unwrap_or_default().to_owned();
    let provider = llm
        .target()
        .map_or_else(String::new, |target| target.name().to_owned());
    let model = llm.physical_model().unwrap_or_default();
    // The budget is charged here, where the provider's own numbers are in hand: only the two
    // full-price buckets count, and only on a request the provider actually served. The charge id
    // is the exchange's own trace id — stable across the request's logging calls, so a retried
    // write bills once. A charge the store cannot confirm is a fact the operator needs: it is
    // logged at warn and never silently dropped, because usage that was served but not recorded
    // makes the budget lie about what was spent.
    if let (Some(input), Some(output)) = (
        usage.get(crate::llm::TokenKind::Input),
        usage.get(crate::llm::TokenKind::Output),
    ) && let Some(store) = ctx.budget_store()
    {
        let charge_id = crate::quota::ChargeId(ctx.trace_id().into());
        if let Err(error) = crate::quota::charge_request(
            store,
            ctx,
            charge_id,
            input,
            output,
            std::time::SystemTime::now(),
        )
        .await
        {
            tracing::warn!(
                target: "nico_http::tokens",
                route_id = %route_id,
                error = %error,
                "the token charge could not be confirmed; the shared window may under-count"
            );
        }
    }
    for (kind, count) in usage.reported() {
        tracing::info!(
            target: "nico_http::tokens",
            route_id = %route_id,
            llm.provider = %provider,
            llm.model = %model,
            llm.tokens.kind = kind.name(),
            llm.tokens.count = count,
            "LLM token usage"
        );
        ::metrics::counter!(
            "gateway_llm_tokens_total",
            "route_id" => route_id.clone(),
            "provider" => provider.clone(),
            "model" => model.clone(),
            "kind" => kind.name(),
        )
        .increment(count);
    }
}
