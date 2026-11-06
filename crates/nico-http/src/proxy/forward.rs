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

use std::{io, net::IpAddr};

use nico_core::{Document, RouteTarget, Upstream};
use pingora::{http::RequestHeader, proxy::Session};

use super::failure::abort;
use crate::{context::HttpContext, error::HttpError, upstream::Destination};

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
/// 3. the document upstream the route names, if the document has it;
/// 4. a refusal. A provider target with no front-end is [`HttpError::ProviderNotConfigured`], because
///    this crate does not know provider semantics and guessing an address would be worse than failing;
///    an upstream name the document lacks is [`HttpError::UpstreamFailed`], which reference validation
///    makes unreachable for a published document.
///
/// The two are ordered, never mixed. A recorded destination wins outright, and step 2's headers are
/// written to whichever peer won — so the day a filter can record a destination for a *provider* route,
/// that filter must also decide what credential goes with it. Nothing exposes that today
/// (`PluginContext` has no destination setter, and the only writer is the crate's own request hook), and
/// this note is the condition under which that stops being true.
///
/// # Errors
/// Returns the failure that explains why no destination exists. It is never a fallback: the request
/// ends with the response the error maps to.
pub(super) fn destination(ctx: &HttpContext) -> Result<Destination, HttpError> {
    if let Some(destination) = ctx.destination() {
        return Ok(destination.clone());
    }
    if let Some(llm) = ctx.llm() {
        return Ok(llm.route().destination().clone());
    }

    let Some(snapshot) = ctx.snapshot() else {
        return Err(HttpError::NotReady);
    };
    match ctx.target() {
        None => Err(HttpError::NotReady),
        Some(RouteTarget::Upstream(name)) => {
            let Some(upstream) = find_upstream(snapshot.document(), name) else {
                return Err(HttpError::UpstreamFailed {
                    source: Box::new(io::Error::other(format!(
                        "the matched route names upstream `{name}`, which the document does not contain"
                    ))),
                });
            };
            Ok(Destination::from(upstream))
        }
        Some(RouteTarget::Provider(name)) => {
            Err(HttpError::ProviderNotConfigured { name: name.clone() })
        }
        // A target kind this build does not know: treat it like a provider with no front-end
        // rather than guessing an address.
        Some(target) => Err(HttpError::ProviderNotConfigured {
            name: target.name().to_owned(),
        }),
    }
}

/// Returns the upstream named `name` from a document, if it has one.
fn find_upstream<'a>(document: &'a Document, name: &str) -> Option<&'a Upstream> {
    document
        .upstreams()
        .iter()
        .find(|upstream| upstream.name() == name)
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
