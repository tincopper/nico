//! Forwarding decisions: which peer a route means, and what the wire head the upstream sees may say.
//!
//! Two rules live here. First, a request reaches a peer only through something that understood the
//! route's target: a plugin that resolved it, or the document entry the target names. Nothing is
//! guessed, and the decision is made once — [`destination`] is the single place that answers "where
//! does this request go", so the hook that connects and the hook that rewrites `Host` cannot
//! disagree. Second, the *framing* of the request is decided by the data plane, not by a plugin.
//!
//! # Who strips and who frames
//!
//! Pingora's `HttpUpstreamRequestPolicy` (default: everything on) removes the hop-by-hop headers and
//! the ones a `Connection` header nominates, and `finalize_h1_upstream_request_framing` adds chunked
//! framing when the request has a body and neither a length nor a transfer coding. Both run around our
//! hooks, so this crate no longer carries a hop-by-hop list of its own: forwarding one would require
//! defeating the framework, which is the stricter implementation by construction.
//!
//! The one thing the framework cannot decide is what a *plugin* did to the headers while the request
//! was being filtered. A plugin may rewrite headers freely, `content-length` included, and a wrong
//! length is not corrected by the finalizer — the write path picks its writer from the header. That is
//! why [`Framing`] snapshots the two framing headers before the chains run and restores them
//! afterwards: framing is the data plane's.

use std::{io, net::IpAddr, sync::Arc, time::Instant};

use nico_core::RouteTarget;
use pingora::{http::RequestHeader, proxy::Session};

use super::failure::abort;
use crate::{
    context::{HttpContext, TrafficWalk},
    error::HttpError,
    upstream::{Destination, pools::Selected},
};

/// The two request headers that decide how the request body is framed.
///
/// The pair is snapshotted and restored together: `content-length` and `transfer-encoding` are
/// mutually exclusive framings, so restoring one without the other could describe a body twice.
const FRAMING_HEADERS: [&str; 2] = ["content-length", "transfer-encoding"];

/// What the client declared about the request body's framing.
///
/// Taken before any plugin runs and restored after the last chain, because a plugin that changes
/// `content-length` must not change how the data plane frames what it sends on. A side effect worth
/// having: the value the upstream normalizer and the body reader agree on is the client's, not a
/// plugin's.
#[derive(Debug)]
pub(super) struct Framing {
    /// Every value of each framing header, in the order the client sent them, as raw bytes.
    ///
    /// Bytes rather than a header-value type: this crate does not name the HTTP framework's value
    /// type anywhere else, and restoring a value must not require re-encoding it.
    values: [Vec<Vec<u8>>; FRAMING_HEADERS.len()],
}

impl Framing {
    /// Records the framing headers of `request`.
    pub(super) fn snapshot(request: &RequestHeader) -> Self {
        Self {
            values: FRAMING_HEADERS.map(|name| {
                request
                    .headers
                    .get_all(name)
                    .iter()
                    .map(|value| value.as_bytes().to_vec())
                    .collect()
            }),
        }
    }

    /// Puts the recorded headers back, removing whatever a plugin left in their place.
    ///
    /// A header that was absent stays absent: "the client declared nothing" is a fact, and restoring a
    /// plugin's value would let a plugin decide the framing by omitting it.
    pub(super) fn restore(&self, request: &mut RequestHeader) {
        for (name, values) in FRAMING_HEADERS.iter().zip(&self.values) {
            let name = *name;
            request.remove_header(name);
            for value in values {
                // A recorded value came out of a header map, so it is valid by construction. The
                // guard's whole job is to leave the request forwardable, so a failure is reported and
                // the chain continues rather than panicking in a hook.
                if let Err(error) = request.append_header(name, value.clone()) {
                    tracing::debug!(
                        header = name,
                        error = %error,
                        "a framing header could not be restored"
                    );
                }
            }
        }
    }
}

/// Returns the address the downstream connection arrived from, when the transport knows it.
///
/// Two callers read this, and both must read the *connection's* address rather than a header: the
/// forwarding path states it in `X-Forwarded-For`, and the request context records it so a policy
/// that keys on the caller never has to trust something the caller wrote. `None` means the transport
/// exposes no peer address (a Unix socket, a session a test built itself).
pub(super) fn peer_ip(session: &Session) -> Option<IpAddr> {
    session
        .client_addr()
        .and_then(pingora::protocols::l4::socket::SocketAddr::as_inet)
        .map(|address| address.ip())
}

/// Returns the destination the request goes to, without touching the clock or the network.
///
/// Precedence is the contract:
///
/// 1. a destination a filter recorded — that filter owns the target kind the route named, and this
///    crate must not second-guess it;
/// 2. the LLM front-end the route was published with, which resolved the provider's address when the
///    document was compiled — the same decision, made earlier and by the crate that understands the
///    provider;
/// 3. one member of the document upstream the route names, chosen by that upstream's strategy from
///    the member list this request's own generation published. Only members the health machine admits
///    are candidates, so a member ejected for failing is not chosen by a strategy that prefers it, and
///    asking a member for admission is what claims the single half-open probe;
/// 4. a refusal. A provider target with no front-end is [`HttpError::ProviderNotConfigured`], because
///    this crate does not know provider semantics and guessing an address would be worse than failing;
///    an upstream with no member left to choose — because every member is ejected, or because the
///    generation published none — is [`HttpError::UpstreamFailed`]. The refusal happens before a socket
///    is opened: sending the request to a member just judged failed would stack another timeout on the
///    failure the caller is already paying for.
///
/// This is the **only** selection point in the data plane. A request's retries stay on the member
/// they selected — the recorded destination wins above — so no second call can move a round to a
/// machine the first round never reached.
///
/// The two halves are ordered, never mixed. A recorded destination wins outright, and step 2's
/// headers are written to whichever peer won — so the day a filter can record a destination for a
/// *provider* route, that filter must also decide what credential goes with it. Nothing exposes that
/// today (`PluginContext` has no destination setter, and the only writer is the crate's own request
/// hook), and this note is the condition under which that stops being true.
///
/// # Errors
/// Returns the failure that explains why no destination exists. It is never a fallback: the request
/// ends with the response the error maps to.
pub(super) fn destination(ctx: &mut HttpContext) -> Result<Destination, HttpError> {
    // A recorded destination is the peer this request connected to, and it belongs to the target it
    // addressed. An LLM round that moved to another chain member must not reuse it — the head would go
    // to the endpoint the request just failed on — so a converting route always re-resolves below and
    // only the front-end's own target can supply the peer.
    if let Some(destination) = ctx.destination()
        && ctx.destination_still_describes_the_attempt()
    {
        return Ok(destination.clone());
    }
    if let Some(llm) = ctx.llm() {
        // The walk selected a target before this hook ran, and that target is the one the head is
        // written to: a fallback's host differs from the route's primary, so reading the route here
        // would send the request back to the endpoint it just failed on.
        if let Some(target) = llm.target() {
            return Ok(target.destination().clone());
        }
        return Ok(llm.route().destination().clone());
    }

    // Selected inside a shared borrow of the context, and recorded after it ends: the member's
    // in-flight count has to be held by the context the borrow came from.
    let selected = select_member(ctx)?;
    let destination = selected.destination;
    ctx.hold_connection(selected.guard);
    Ok(destination)
}

/// Chooses one member of the upstream this request's route names.
///
/// The target, the generation's member list and the caller's address all come from the context, so the
/// choice is made against the document the request was routed with — not against whatever the store
/// holds now. A route that distributes across several upstreams picks the upstream first, from its
/// own pool, **once per request** — a retry round moves to another member of the same upstream, never
/// re-draws the pool — and the upstream's member pool then picks the machine among the members this
/// request has not already met.
fn select_member(ctx: &mut HttpContext) -> Result<Selected, HttpError> {
    let Some(target) = ctx.target() else {
        return Err(HttpError::NotReady);
    };
    let RouteTarget::Upstream(name) = target else {
        return Err(match target {
            RouteTarget::Provider(name) => HttpError::ProviderNotConfigured { name: name.clone() },
            // A target kind this build does not know: treat it like a provider with no front-end
            // rather than guessing an address.
            other => HttpError::ProviderNotConfigured {
                name: other.name().to_owned(),
            },
        });
    };
    let Some(selection) = ctx.selection() else {
        return Err(HttpError::NotReady);
    };
    let now = Instant::now();

    // The walk, when this request opened one, holds the upstream its pool chose: sticky for the
    // walk's lifetime. A request without a walk either has no pool or makes its first round now,
    // which is when the pool's draw happens — and when a walk without an upstream names its target.
    let upstream = match ctx.traffic_walk() {
        Some(walk) => walk.upstream().clone(),
        None => match ctx.route_id().and_then(|route| selection.route_pool(route)) {
            Some(pool) => selection
                .pick_upstream(pool, ctx.client_ip(), now)
                .ok_or_else(|| no_pooled_member(target))?
                .clone(),
            None => Arc::from(name.as_str()),
        },
    };
    let Some(state) = selection.upstream(&upstream) else {
        return Err(no_member(&upstream));
    };
    // The members this request already met are out of the candidates, so a failover round cannot be
    // sent to the machine that just failed.
    let tried = ctx
        .traffic_walk()
        .map(|walk| walk.tried().to_vec())
        .unwrap_or_default();
    let client = ctx.client_ip();
    let selected = state
        .select_excluding(&tried, client, now)
        .ok_or_else(|| no_member(&upstream))?;
    let member = selected.guard.member_id().clone();

    // The member this round selected joins the walk's tried set, and a request without a walk opens
    // one here: its first attempt is spent from this moment, which is what makes the route's budget
    // "including the first".
    match ctx.traffic_walk_mut() {
        Some(walk) => walk.record_tried(member),
        None => {
            let mut walk = TrafficWalk::new(upstream, ctx.failover_budget());
            walk.record_tried(member);
            ctx.begin_traffic_walk(walk);
        }
    }
    Ok(selected)
}

/// Builds the refusal for an upstream with nothing left to select.
///
/// `502 upstream_unreachable` is the existing row for "the peer cannot be reached", and it is the
/// honest one here: the gateway has just decided that no member is available, so refusing is what
/// keeps the failure visible instead of stacking another timeout on it. The message names the upstream,
/// never a member address.
fn no_member(name: &str) -> HttpError {
    HttpError::UpstreamFailed {
        source: Box::new(io::Error::other(format!(
            "upstream `{name}` has no available member"
        ))),
    }
}

/// Builds the refusal for a pooled route with nothing left to select.
///
/// Every upstream the route's pool could name is out of rotation, so the honest answer is the same
/// `502 upstream_unreachable` row — the message names the route, because there is no single upstream
/// to blame.
fn no_pooled_member(target: &RouteTarget) -> HttpError {
    HttpError::UpstreamFailed {
        source: Box::new(io::Error::other(format!(
            "route `{}` has no upstream with an available member",
            target.name()
        ))),
    }
}

/// Sets a request header, converting the header-name failure into a recorded data-plane error.
pub(super) fn set_header(
    request: &mut RequestHeader,
    name: &'static str,
    value: &str,
    ctx: &mut HttpContext,
) -> pingora::Result<()> {
    request
        .insert_header(name, value)
        .map_err(|error| abort(ctx, HttpError::Internal { source: error }))
}

/// Appends the client address to any existing `X-Forwarded-For`, returning the new value.
///
/// A request with no peer address (only possible for a session Pingora built itself) keeps the
/// values that are already there instead of inventing one.
pub(super) fn forwarded_for(
    request: &RequestHeader,
    client: Option<std::net::IpAddr>,
) -> Option<String> {
    let existing = request
        .headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok());
    let client = client.map(|address| address.to_string());
    match (existing, client) {
        (Some(existing), Some(client)) => Some(format!("{existing}, {client}")),
        (Some(existing), None) => Some(existing.to_owned()),
        (None, Some(client)) => Some(client),
        (None, None) => None,
    }
}
