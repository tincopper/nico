//! The active probe executor: one health check, end to end, against one member.
//!
//! The scheduler owns *when*; this module owns *how*. One [`run_once`] pass walks the current
//! generation's members, claims each due member's probe slot, sends the check the document
//! described, and reports the verdict to the member's active state. Everything a request path
//! would attach — credentials, headers, plugins, quotas, the request log and the request metrics
//! — is deliberately absent: a probe asks one question, "does this machine answer", and its only
//! witnesses are the member's own counters and the probe metric.
//!
//! The transport is the same one requests use. The connector is built from the process's own TLS
//! trust facts, the peer is the member's address carrying the upstream's logical host as its SNI,
//! and the session is Pingora's client session — so a probe that passes means "the same TLS the
//! requests see verifies this member", not "some separate checker trusted it".
//!
//! # Status
//!
//! Driven by nico-server's `upstream-health` BackgroundService (phase 4); until that lands, the
//! module's surface is exercised by its tests only.
//!
//! Probes run concurrently *across* members (each is its own slot) but never twice on one member
//! (`begin_probe` refuses a second claim). The future returned here is driven by the scheduler;
//! nothing in this module spawns.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use pingora::upstreams::peer::HttpPeer;

use crate::upstream::pools::{
    SelectionTable,
    active::{ActiveTransition, ProbeOutcome},
};

/// How one probe ended, as the metric labels it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeResult {
    /// The member answered within the reading the check stated.
    Ok,
    /// The connection could not be established.
    Connect,
    /// The connection or the answer exceeded the check's budget.
    Timeout,
    /// The member answered with a 5xx.
    Status5xx,
    /// The member answered with a status the check's reading does not accept (a 4xx under
    /// `expect: 2xx`).
    StatusMismatch,
}

impl ProbeResult {
    /// The `result` value the probe metric carries.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Connect => "connect",
            Self::Timeout => "timeout",
            Self::Status5xx => "5xx",
            Self::StatusMismatch => "status_mismatch",
        }
    }

    /// The verdict this result gives the member's active counters.
    const fn outcome(self) -> ProbeOutcome {
        match self {
            Self::Ok => ProbeOutcome::Passed,
            Self::Connect | Self::Timeout | Self::Status5xx | Self::StatusMismatch => {
                ProbeOutcome::Failed
            }
        }
    }
}

/// One pass over the generation: every due member is probed, every verdict recorded.
///
/// `now` is the tick's time; the scheduler passes its own clock so the pass is one decision
/// about a coherent instant.
#[cfg(test)]
mod tests;

pub(crate) async fn run_once(table: &SelectionTable, now: Instant) {
    for upstream in table.iter_upstreams() {
        let Some(check) = upstream.health_check() else {
            continue;
        };
        let interval = Duration::from_millis(u64::from(check.interval_ms()));
        for member in upstream.probe_targets() {
            let state = member.state();
            let Some(active) = state.active_health() else {
                continue;
            };
            // Due when the member has never been probed, or its interval has passed since the
            // last one finished. The no-traffic cadence is the scheduler's tick decision; the
            // per-member floor stays the written interval so a member is never probed faster
            // than its document states.
            if !active.due(interval, now) {
                continue;
            }
            // The fingerprint reset a config change asked for is applied at claim time: the new
            // checker starts from its own zero, whichever generation scheduled this probe.
            let fingerprint = super::active::CheckFingerprint::of(check);
            let reset = active.under().is_some_and(|under| under != fingerprint);
            if !active.begin_probe(reset) {
                continue; // A probe is already out for this member.
            }
            active.counted_under(fingerprint);

            let result = probe_member(upstream.scheme(), upstream.authority(), member, check).await;
            emit_probe_metric(
                metrics::SharedString::from(Arc::clone(upstream.name_handle())),
                metrics::SharedString::from(Arc::clone(state.id().name_handle())),
                result,
            );
            let transition = active.finish_probe(
                result.outcome(),
                check.unhealthy_threshold(),
                check.healthy_threshold(),
            );
            log_transition(upstream.name(), state.id().name(), transition, check);
        }
    }
}

/// Sends one check and returns the result's verdict.
async fn probe_member(
    scheme: nico_core::Scheme,
    authority: &str,
    member: &crate::upstream::pools::Member,
    check: &nico_core::HealthCheck,
) -> ProbeResult {
    let id = member.state().id();
    let timeout = Duration::from_millis(u64::from(check.timeout_ms()));
    let host_port = (id.address().to_string(), id.port());
    let Ok(mut address) = tokio::net::lookup_host(host_port).await else {
        return ProbeResult::Connect;
    };
    let Some(address) = address.next() else {
        return ProbeResult::Connect;
    };
    let peer = HttpPeer::new(
        address,
        scheme == nico_core::Scheme::Https,
        authority.to_owned(),
    );

    // The connector is built per probe on purpose: it carries no pooled state, so a check can
    // never reuse a connection a previous probe (or a request) left half-healthy.
    let connector = pingora::connectors::http::Connector::new(None);
    let connected = tokio::time::timeout(timeout, connector.get_http_session(&peer)).await;
    let (mut session, _reused) = match connected {
        // The check's budget ran out: a timeout, not the member's fault classification.
        Err(_elapsed) => return ProbeResult::Timeout,
        // The wire refused or broke: a connect failure.
        Ok(Err(_wire)) => return ProbeResult::Connect,
        Ok(Ok(session)) => session,
    };
    session.set_read_timeout(Some(timeout));
    session.set_write_timeout(Some(timeout));

    let host = if is_default_port(scheme, id.port()) {
        authority.to_owned()
    } else {
        format!("{authority}:{}", id.port())
    };
    // A header the validated method/path/host cannot build is a probe that never became a
    // question. It is a probe-side defect, not a member's answer, so it reports as Connect —
    // and the schema tests prove it cannot happen for a parsed check.
    let Ok(mut request) =
        pingora::http::RequestHeader::build(check.method().as_str(), check.path().as_bytes(), None)
    else {
        return ProbeResult::Connect;
    };
    if request.insert_header("Host", host).is_err()
        || request.insert_header("Connection", "close").is_err()
        || request
            .insert_header("User-Agent", "nico-health-probe")
            .is_err()
    {
        return ProbeResult::Connect;
    }

    if session
        .write_request_header(Box::new(request))
        .await
        .is_err()
    {
        return ProbeResult::Connect;
    }
    // H1 sessions read informational responses repeatedly; the first final head is the verdict.
    loop {
        match tokio::time::timeout(timeout, session.read_response_header()).await {
            Err(_elapsed) => return ProbeResult::Timeout,
            Ok(Err(_wire)) => return ProbeResult::Connect,
            Ok(Ok(())) => {
                let Some(response) = session.response_header() else {
                    continue; // informational: keep reading within the same budget
                };
                return judge(response.status.as_u16(), check);
            }
        }
    }
}

/// Judges one answered status against the check's reading.
fn judge(status: u16, check: &nico_core::HealthCheck) -> ProbeResult {
    match check.expect() {
        nico_core::ExpectedStatus::Non5xx => {
            if (500..600).contains(&status) {
                ProbeResult::Status5xx
            } else {
                ProbeResult::Ok
            }
        }
        nico_core::ExpectedStatus::S2xx => {
            if (200..300).contains(&status) {
                ProbeResult::Ok
            } else if (500..600).contains(&status) {
                ProbeResult::Status5xx
            } else {
                ProbeResult::StatusMismatch
            }
        }
    }
}

/// Whether `port` is the scheme's default, so the Host header omits it.
fn is_default_port(scheme: nico_core::Scheme, port: u16) -> bool {
    matches!(
        (scheme, port),
        (nico_core::Scheme::Http, 80) | (nico_core::Scheme::Https, 443)
    )
}

/// Counts one finished probe.
fn emit_probe_metric(
    upstream: metrics::SharedString,
    endpoint: metrics::SharedString,
    result: ProbeResult,
) {
    metrics::counter!(
        "gateway_upstream_probe_total",
        "upstream" => metrics::SharedString::from(upstream),
        "endpoint" => endpoint,
        "result" => result.as_str(),
    )
    .increment(1);
}

/// Logs the active verdict a probe moved, at the level the operator's action implies.
fn log_transition(
    upstream: &str,
    endpoint: &str,
    transition: ActiveTransition,
    check: &nico_core::HealthCheck,
) {
    match transition {
        ActiveTransition::None => {}
        ActiveTransition::Fell => tracing::warn!(
            upstream = %upstream,
            upstream.endpoint = %endpoint,
            active.unhealthy_threshold = check.unhealthy_threshold(),
            "upstream member failed its active health check and left rotation"
        ),
        ActiveTransition::Rose => tracing::info!(
            upstream = %upstream,
            upstream.endpoint = %endpoint,
            active.healthy_threshold = check.healthy_threshold(),
            "upstream member passed its active health check and returned"
        ),
    }
}
