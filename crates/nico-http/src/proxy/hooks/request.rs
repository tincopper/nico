//! The request direction: decide whether the request is served, and by which peer.
//!
//! The order inside [`request_filter`] is the whole publication model in one function:
//!
//! 1. read the published state **once**;
//! 2. set the document-level budget, because a global plugin runs before a route exists and still runs
//!    inside the request's remaining time;
//! 3. snapshot the framing headers, so a plugin cannot decide how the body is framed;
//! 4. run the **global** chain — it is the only stage a request that matches no route still reaches;
//! 5. match a route against the headers the global chain left;
//! 6. record the route, tighten the budget to it, resolve the LLM front-end a provider route needs, and
//!    resolve its chain (both fail-closed);
//! 7. run the **route** chain;
//! 8. restore the framing headers, and only then answer.
//!
//! Restoring last is deliberate: every early exit above is a rejection or a synthesised response, and
//! doing the restore in one place after the verdict is what keeps an early exit from skipping it.

use std::time::{Duration, Instant};

use pingora::{Error, http::RequestHeader, proxy::Session, upstreams::peer::HttpPeer};
use tokio::time::Instant as TokioInstant;

use super::{
    super::{
        HttpApp,
        failure::{abort, proxy_error},
        forward::{Framing, destination, forwarded_for, peer_ip, set_header},
    },
    llm,
};
use crate::{
    budget,
    context::{FailureStage, HttpContext},
    error::HttpError,
    plugin::ResponseData,
    route,
    state::{self, HttpState},
    upstream::{self, pools::MemberFailure},
};

/// What the request phase decided, before anything is written.
///
/// The three outcomes are the three answers a request can get before an upstream is contacted, and
/// they are decided as a value so the framing guard can be restored exactly once, whatever happened.
pub(super) enum Verdict {
    /// Nothing answered: forward the request.
    Forward,
    /// A plugin answered instead of the upstream.
    Answer(ResponseData),
    /// The request is refused with this failure.
    Refuse(HttpError),
}

/// Routes the request, then runs the policies that may answer it.
pub(super) async fn request_filter(
    app: &HttpApp,
    session: &mut Session,
    ctx: &mut HttpContext,
) -> pingora::Result<bool> {
    let now = Instant::now();
    let trace_id = app.trace_id(session.req_header());
    ctx.set_trace_id(&trace_id);
    // Read before the immutable borrow of the request header ends: the peer address is a fact of the
    // connection, so a plugin that keys on the caller reads it from the context instead of from a
    // header the caller controls.
    let client_ip = peer_ip(session);

    // `HttpStateHandle::servable` is the named form of this check; this branch takes the snapshot
    // itself, so it reads `current()` once rather than asking the predicate and reading again.
    let Some(state) = app.state.current() else {
        return app.reject(session, ctx, HttpError::NotReady).await;
    };

    // The document budget first: the global chain below runs before any route is known, and a stage
    // with no deadline would be the one stage no budget covers.
    ctx.begin(state.snapshot(), session.req_header(), now);
    // The member list travels with the snapshot: a request that selected from another generation's
    // list would connect to an address its own document does not name.
    ctx.set_selection(state.selection());
    // The budget table travels with the request, so the check at peer time and the charge at record
    // time read the same store the process assembled. It lives on the handle, not on the snapshot:
    // a reload replaces the document without resetting what identities have spent.
    ctx.set_budget_store(app.budget_store());
    if let Some(address) = client_ip {
        ctx.set_client_ip(address);
    }

    // Framing belongs to the data plane. Snapshot before any plugin can change it, and restore after
    // the last chain: a wrong `content-length` is not corrected later, because the write path picks its
    // writer from that header.
    let framing = Framing::snapshot(session.req_header());
    let verdict = route_request(session, ctx, &state).await;
    framing.restore(session.req_header_mut());

    match verdict {
        Verdict::Forward => Ok(false),
        Verdict::Answer(data) => app.respond(session, ctx, data).await,
        Verdict::Refuse(error) => app.reject(session, ctx, error).await,
    }
}

/// Runs the two request-scope chains around routing and returns what to do next.
///
/// Every failure is returned as a value rather than propagated, so the caller restores the framing
/// headers once, after the request can no longer change.
async fn route_request(session: &mut Session, ctx: &mut HttpContext, state: &HttpState) -> Verdict {
    // 1. The global chain. A request that matches no route never reaches another stage, which is why
    //    a global plugin is the only kind that can refuse an unroutable request.
    let global = state.global().clone();
    // Recorded before it runs, so a request this chain refuses still reaches `on_logging` and the
    // request record attributes the decision to the plugin that made it.
    ctx.set_chains(global.clone(), None);
    match global.run_request(ctx, session.req_header_mut()).await {
        Ok(None) => {}
        Ok(Some(data)) => return Verdict::Answer(data),
        Err(error) => return Verdict::Refuse(error),
    }

    // 2. Routing sees the headers the global chain left: a plugin may rewrite the target or a header a
    //    route matches on, and the route it meant is the one to use.
    let Some(matched) =
        route::match_route(session.req_header(), state.snapshot().document().routes())
    else {
        return Verdict::Refuse(HttpError::RouteNotFound);
    };
    ctx.bind_route(matched);

    // 3. The LLM front-end, before anything else reads the body: a provider route the document published
    //    without one is refused, never forwarded with the caller's bytes and never resolved against a
    //    guessed address. The lookup is by route id, so the plan is a publication decision like the
    //    chain below it.
    match state::llm_for_route(state, matched) {
        Ok(Some(front_end)) => ctx.set_llm(front_end),
        Ok(None) => {}
        Err(error) => return Verdict::Refuse(error),
    }

    // 4. Resolve the route's chain before anything else can fail: whether the route *names* plugins is
    //    read inside, because a route that names one and finds no chain must not be forwarded — a
    //    missing entry is an authentication bypass, not "no policy".
    let chain = match state::chain_for_route(state, matched) {
        Ok(chain) => chain,
        Err(error) => return Verdict::Refuse(error),
    };

    // 5. A declared length over the route's limit is refused before a plugin runs: the body will never
    //    be accepted, so no policy has a decision to make about it.
    let declared = budget::declared_length(session.req_header());
    if let (Some(limit), Some(declared)) = (ctx.body_limit(), declared)
        && budget::over_limit(limit, declared)
    {
        return Verdict::Refuse(HttpError::BodyTooLarge { limit });
    }

    // 6. Record what this request runs and how it is framed. Both flags are publication decisions, read
    //    here because the hooks that consume them run per chunk and may not re-read configuration. The
    //    route chain joins the global one recorded above: both come from the same `HttpState`.
    ctx.set_chains(global, chain.clone());
    ctx.set_buffered(state.buffered().contains(matched.id()));
    ctx.set_response_rewrite(state.rewrites_response(matched.id()));

    // 7. The route chain: same generation, after routing, so it sees the route id and the budget the
    //    route tightened. Only the request stage runs on an LLM route; the body and response stages are
    //    skipped in their hooks, because those bytes belong to a protocol a policy cannot see.
    let Some(chain) = chain else {
        return Verdict::Forward;
    };
    match chain.run_request(ctx, session.req_header_mut()).await {
        Ok(None) => Verdict::Forward,
        Ok(Some(data)) => Verdict::Answer(data),
        Err(error) => Verdict::Refuse(error),
    }
}

/// Turns the matched target into a peer, bounded by the request's remaining budget.
///
/// The peer is decided once and recorded in the context: the `Host` rewrite that follows reads the same
/// destination instead of resolving the target a second time, so a plugin-chosen peer and the header
/// sent to it cannot disagree.
pub(super) async fn upstream_peer(
    _app: &HttpApp,
    _session: &mut Session,
    ctx: &mut HttpContext,
) -> pingora::Result<Box<HttpPeer>> {
    let now = Instant::now();
    // The token budget is checked here because it is the last point before anything upstream
    // happens: identity is already established by the auth plugins, and the route that declares the
    // budget is matched. A request without an identity is not bounded — the record says so.
    // The store is None only for a request that never reached service, which `upstream_peer` cannot
    // be called for; the `if let` keeps the accessor honest without unwrapping.
    if let Some(store) = ctx.budget_store()
        && let Err(error) = crate::quota::check_request(store, ctx, TokioInstant::now())
    {
        return Err(abort(ctx, error));
    }
    // An expired budget spends no resolver query and opens no socket: the answer is already known, and
    // it is the request's own clock that decided it.
    let budget = ctx.remaining(now).unwrap_or_default();
    if budget.is_zero() {
        let error = ctx.deadline_error();
        return Err(abort(ctx, error));
    }

    // A converting route selects its first target here: the selection needs only the walk and the
    // clock, so the head this attempt writes is the target's own — a fallback's host differs from the
    // route's primary, and only the walk knows which one this attempt reached.
    if let Err(error) = select_attempt(ctx, TokioInstant::now()) {
        return Err(abort(ctx, error));
    }

    // Every round resolves its own peer: a recorded destination belongs to the attempt that
    // recorded it, and a round that moved to another target must not reuse it.
    let chosen = destination(ctx).map_err(|error| abort(ctx, error))?;
    let read_budget = attempt_read_budget(ctx, budget);
    let peer = match upstream::peer(&chosen, budget, read_budget).await {
        Ok(peer) => peer,
        Err(error) => {
            // The name did not resolve, so this member never accepted a connection: the failure is
            // the member's own, and it is the first of the three counting points (the other two are
            // `fail_to_connect` and the response head). Nothing else in this path may count it, or a
            // single request would spend two failures of the member's budget.
            ctx.member_failed(MemberFailure::Connect, Instant::now());
            return Err(failed_target(ctx, error));
        }
    };
    ctx.set_destination(chosen);
    Ok(peer)
}

/// Returns how long one read may take, before the member is worth replacing.
///
/// A route that states `ttft_timeout_ms` and asked for a stream gets that budget instead of the
/// request's remaining time: the wait for an answer is the thing an operator wants bounded, and the
/// whole-request budget is the ceiling that still applies. Everything else keeps the request budget,
/// which is what every route had before the field existed.
fn attempt_read_budget(ctx: &HttpContext, budget: Duration) -> Duration {
    let Some(llm) = ctx.llm() else {
        return budget;
    };
    // The budget applies whenever the route states one, not only once the body has been decoded: this
    // hook runs *before* the client's body arrives, so the streaming flag is not known yet, and gating
    // on it would leave the one attempt that most needs the bound — the first, meeting a member that
    // never answers — unbounded. The consequence is stated in the field's documentation: on a route
    // that states it, the bound is what one read may take, streamed or buffered.
    llm.target()
        .and_then(|target| target.ttft())
        .map_or(budget, |ttft| ttft.min(budget))
}

/// Classifies a target whose address failed before a connection existed.
///
/// A resolution failure — and a scheme this build cannot speak — is this target's own transport
/// failure: the walk classifies it exactly like any other, and when it holds another attempt the
/// data plane sets the retry bit itself, because Pingora's loop takes neither `fail_to_connect` nor
/// `error_while_proxy` for an `upstream_peer` error of its own.
///
/// No response can have been committed here — this hook runs before the round's head is written — and
/// no body byte was dispatched, so the gate needs no replay buffer.
fn failed_target(ctx: &mut HttpContext, error: HttpError) -> Box<Error> {
    if super::llm::retry_admitted(ctx, false, FailureStage::BeforeDispatch) {
        let mut wrapped = proxy_error(&error);
        wrapped.set_retry(true);
        return wrapped;
    }
    // The walk refused, or the route has no walk: the failure is the request's outcome.
    abort(ctx, error)
}

/// Asks the request's walk to select its first target, and records the head facts it chose.
///
/// The skipped targets are logged once, here: a selection that skipped them is the only place that
/// knows they were passed over, and the record belongs to the attempt that follows. The record is
/// drained before the selection is judged, because a selection that refused — every target open —
/// is exactly when the skips must still reach the log: the round that would have logged them on the
/// way forward never runs.
fn select_attempt(ctx: &mut HttpContext, now: TokioInstant) -> Result<(), HttpError> {
    let Some(llm) = ctx.llm_mut() else {
        return Ok(());
    };
    // The first round selects; a later round already holds the member its classification moved to.
    // The distinction is the walk's own, and asking twice would spend a budget it already accounted
    // for. Read before the mutable borrow below, so the two do not overlap.
    let fresh = !llm.has_target();
    let Some(attempt) = llm.attempt_mut() else {
        return Ok(());
    };
    let selection = fresh.then(|| attempt.select(now));
    let skipped = attempt.take_skipped();
    let number = attempt.attempt_number();
    if let Some(Err(error)) = selection {
        // No attempt follows: the number is the one the refused round would have started.
        for name in skipped {
            tracing::info!(
                llm.target = %name,
                llm.attempt = number,
                llm.outcome = "skipped",
                "LLM attempt skipped"
            );
        }
        return Err(error);
    }
    let target = attempt.target_view();
    let name = target.name().to_owned();
    llm.advance_target(target);
    for name in skipped {
        tracing::info!(
            llm.target = %name,
            llm.attempt = number,
            llm.outcome = "skipped",
            "LLM attempt skipped"
        );
    }
    // The attempt starts here: its facts and its clock are what a failure later in the round
    // reports.
    llm.note_attempt_started(&name, number);
    Ok(())
}

/// Rewrites the request headers for the upstream, and frames a buffered body as chunked.
///
/// What this hook no longer does is strip hop-by-hop headers: Pingora's
/// `HttpUpstreamRequestPolicy` removes the standard set and the ones a `Connection` header nominates,
/// and it rejects a request that tries to nominate `Host` or `X-Forwarded-*`. Reimplementing that here
/// would be a second, weaker copy of a rule the framework already applies before this hook runs.
pub(super) async fn upstream_request_filter(
    app: &HttpApp,
    session: &mut Session,
    request: &mut RequestHeader,
    ctx: &mut HttpContext,
) -> pingora::Result<()> {
    // This hook is a round's boundary: it runs once per attempt, before that round's head is written
    // and before any of its body tasks. A retry round receives the client's bytes again from the
    // framework's replay buffer, so the converting route's accumulation and its count restart here —
    // the only place that can make each client byte count exactly once.
    if ctx.converting().is_some() {
        ctx.begin_attempt_body();
    }
    // Both reads borrow the downstream session immutably; the header below is a different object, so
    // the borrows do not overlap.
    let client_host = session.req_header().headers.get("host").cloned();
    let client_addr = peer_ip(session);

    // An LLM route's path and headers were compiled at publication and belong to the provider. They are
    // written before `Host` and `X-Forwarded-*`, so a route can never choose the peer the request is
    // written to; `convert` also drops the caller's credential, since the provider's replaces it.
    llm::apply_upstream_request(request, ctx).map_err(|error| abort(ctx, error))?;

    // `upstream_peer` recorded the destination it connected to, so this hook only reads it.
    let Some(destination) = ctx.destination() else {
        return Err(abort(
            ctx,
            HttpError::Internal {
                source: Box::new(std::io::Error::other(
                    "no destination was recorded before the request was forwarded",
                )),
            },
        ));
    };
    let host = upstream::host_header(destination);
    if let Some(host) = client_host.as_ref().and_then(|value| value.to_str().ok()) {
        set_header(request, "x-forwarded-host", host, ctx)?;
    }
    if let Some(forwarded_for) = forwarded_for(request, client_addr) {
        set_header(request, "x-forwarded-for", &forwarded_for, ctx)?;
    }
    // Only the outermost hop states the scheme: appending a second value produces a list that no
    // server parses. The transport this listener actually uses decides the value; a client that
    // already supplied one keeps it, as before.
    if request.headers.get("x-forwarded-proto").is_none() {
        let scheme = if app.inbound_tls() { "https" } else { "http" };
        set_header(request, "x-forwarded-proto", scheme, ctx)?;
    }
    set_header(request, "host", &host, ctx)?;

    // A buffered route holds every chunk back until the stream ends, and a plugin may replace even an
    // empty body with bytes. The client's declared length therefore describes neither the size nor the
    // existence of the body the upstream will receive, so the data plane states the framing itself.
    //
    // This cannot be conditioned on `session.is_body_empty()`: that is a fact about the *downstream*
    // request, while the buffered body is a publication decision. With a zero-length writer already
    // chosen from the header, bytes a plugin produced on the final call would be dropped or would
    // desynchronise the stream. Chunked framing is valid for an empty body too — it ends with a
    // terminating chunk — so it costs one header and removes the whole class of disagreement.
    //
    // A converted LLM body is the same case with a different author: the prepared bytes are encoded
    // from the caller's document, so the length the client declared describes neither of them.
    // `passthrough` keeps the client's framing, which is the point of it.
    if ctx.buffers_body() || ctx.converting().is_some() {
        request.remove_header("content-length");
        set_header(request, "transfer-encoding", "chunked", ctx)?;
    }

    Ok(())
}
