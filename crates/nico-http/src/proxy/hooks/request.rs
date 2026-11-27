//! Routing and upstream pool selection for one request.
//!
//! Request routing reads one published state, snapshots framing before policies run, then restores it
//! once before any answer. The selected route and its budgets stay on the request context throughout
//! peer selection and forwarding.

use std::time::{Duration, Instant};

use pingora::{Error, proxy::Session, upstreams::peer::HttpPeer};
use tokio::time::Instant as TokioInstant;

use super::{
    super::{
        HttpApp,
        failure::{abort, proxy_error},
        forward::{Framing, destination, peer_ip},
    },
    llm, request_headers,
};
use crate::{
    budget,
    context::{FailureStage, HttpContext},
    error::HttpError,
    plugin::ResponseData,
    route,
    state::{self, HttpState},
    upstream::pools::MemberFailure,
};

pub(super) enum Verdict {
    Forward,
    Answer(ResponseData),
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
    if let Some(host) = session
        .req_header()
        .headers
        .get("host")
        .and_then(|value| value.to_str().ok())
    {
        ctx.capture_client_host(host);
    }
    let peer = peer_ip(session);
    let client_ip = peer.map(|peer| {
        let forwarded_for = session
            .req_header()
            .headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok());
        crate::trusted::real_client_ip(peer, forwarded_for, app.trusted_proxies())
    });
    let Some(state) = app.state.current() else {
        return app.reject(session, ctx, HttpError::NotReady).await;
    };
    ctx.begin(state.snapshot(), session.req_header(), now);
    ctx.set_selection(state.selection());
    ctx.set_budget_store(app.budget_store());
    if let Some(address) = client_ip {
        ctx.set_client_ip(address);
    }

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
async fn route_request(session: &mut Session, ctx: &mut HttpContext, state: &HttpState) -> Verdict {
    let global = state.global().clone();
    ctx.set_chains(global.clone(), None);
    match global.run_request(ctx, session.req_header_mut()).await {
        Ok(None) => {}
        Ok(Some(data)) => return Verdict::Answer(data),
        Err(error) => return Verdict::Refuse(error),
    }

    let Some(matched) =
        route::match_route(session.req_header(), state.snapshot().document().routes())
    else {
        return Verdict::Refuse(HttpError::RouteNotFound);
    };
    ctx.bind_route(matched);
    match state::llm_for_route(state, matched) {
        Ok(Some(front_end)) => ctx.set_llm(front_end),
        Ok(None) => {}
        Err(error) => return Verdict::Refuse(error),
    }
    let chain = match state::chain_for_route(state, matched) {
        Ok(chain) => chain,
        Err(error) => return Verdict::Refuse(error),
    };
    let declared = budget::declared_length(session.req_header());
    if let (Some(limit), Some(declared)) = (ctx.body_limit(), declared)
        && budget::over_limit(limit, declared)
    {
        return Verdict::Refuse(HttpError::BodyTooLarge { limit });
    }
    ctx.set_chains(global, chain.clone());
    ctx.set_buffered(state.buffered().contains(matched.id()));
    ctx.set_response_rewrite(state.rewrites_response(matched.id()));
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
pub(super) async fn upstream_peer(
    _app: &HttpApp,
    _session: &mut Session,
    ctx: &mut HttpContext,
) -> pingora::Result<Box<HttpPeer>> {
    let now = Instant::now();
    if let Some(store) = ctx.budget_store() {
        match crate::quota::check_request(store, ctx, std::time::SystemTime::now()).await {
            Ok(()) => {}
            // The window is spent: the caller waits for the reset.
            Err(Ok(error)) => return Err(abort(ctx, error)),
            // The store could not confirm the admission. The standing answer is fail-closed —
            // a budget that cannot be read is not a budget that allows — carried as its own
            // 503, never as a 429. A deployment that prefers availability opts into fail-open
            // through its store's configuration (phase 2's `on_unavailable`).
            Err(Err(store_error)) => {
                return Err(abort(
                    ctx,
                    HttpError::BudgetStoreUnavailable {
                        source: Box::new(store_error),
                    },
                ));
            }
        }
    }
    let budget = ctx.remaining(now).unwrap_or_default();
    if budget.is_zero() {
        let error = ctx.deadline_error();
        return Err(abort(ctx, error));
    }
    if let Err(error) = select_attempt(ctx, TokioInstant::now()) {
        return Err(abort(ctx, error));
    }

    let chosen = destination(ctx).map_err(|error| abort(ctx, error))?;
    let read_budget = Some(attempt_read_budget(ctx, budget));
    let peer = match crate::upstream::peer(&chosen, budget, read_budget).await {
        Ok(peer) => peer,
        Err(error) => {
            ctx.member_failed(MemberFailure::Connect, Instant::now());
            return Err(failed_target(ctx, error));
        }
    };
    ctx.set_destination(chosen);
    Ok(peer)
}

fn attempt_read_budget(ctx: &HttpContext, budget: Duration) -> Duration {
    let Some(llm) = ctx.llm() else {
        return budget;
    };
    llm.target()
        .and_then(|target| target.ttft())
        .map_or(budget, |ttft| ttft.min(budget))
}

fn failed_target(ctx: &mut HttpContext, error: HttpError) -> Box<Error> {
    if llm::retry_admitted(ctx, false, FailureStage::BeforeDispatch) {
        let mut wrapped = proxy_error(&error);
        wrapped.set_retry(true);
        return wrapped;
    }
    abort(ctx, error)
}

fn select_attempt(ctx: &mut HttpContext, now: TokioInstant) -> Result<(), HttpError> {
    let client = ctx.client_ip();
    let Some(llm) = ctx.llm_mut() else {
        return Ok(());
    };
    let fresh = !llm.has_target();
    let Some(attempt) = llm.attempt_mut() else {
        return Ok(());
    };
    let selection = fresh.then(|| attempt.select(now, client));
    let skipped = attempt.take_skipped();
    let number = attempt.attempt_number();
    if let Some(Err(error)) = selection {
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
    llm.note_attempt_started(&name, number);
    Ok(())
}

/// Rewrites the upstream request head and its framing.
pub(super) async fn upstream_request_filter(
    app: &HttpApp,
    session: &mut Session,
    request: &mut pingora::http::RequestHeader,
    ctx: &mut HttpContext,
) -> pingora::Result<()> {
    request_headers::upstream_request_filter(app, session, request, ctx).await
}
