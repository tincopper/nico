//! Forwarding decisions: which peer a route means, and what the upstream must not see.
//!
//! Two rules live here. First, a request reaches a peer only through something that understood the
//! route's target: a filter that resolved it, or the document entry the target names. Nothing is
//! guessed, and the decision is made once — [`destination`] is the single place that answers "where
//! does this request go", so the hook that connects and the hook that rewrites `Host` cannot
//! disagree. Second, the headers that describe the downstream connection stay behind: a hop-by-hop
//! header forwarded to the next hop describes a connection the client never used, and a `Host` the
//! upstream did not ask for breaks virtual hosting.

use std::io;

use gateway_core::{Document, RouteTarget, Upstream};
use pingora::http::RequestHeader;

use super::failure::abort;
use crate::{context::HttpContext, error::HttpError, upstream::Destination};

/// Headers that describe a single connection and must not be forwarded to the next one.
///
/// The list is RFC 9110's hop-by-hop set plus the de-facto `proxy-*` variants. The headers a request
/// *nominates* in its own `Connection` header are a second, open-ended set: those are read by
/// [`connection_tokens`] before this list removes the header itself. Pingora does not strip either
/// set on our behalf (it only reads `Connection` to decide keepalive), so forwarding one would leak
/// the client's connection semantics to the upstream.
pub(super) const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
];

/// Most `Connection` tokens one request may nominate.
///
/// Every nominated name is removed from the forwarded headers, so the work per request is bounded by
/// this number rather than by the header's size. A request that nominates more is refused instead of
/// served with some of its connection-scoped headers still attached: half of a removal is exactly
/// the leak the removal exists to prevent, and the bound is far above any real client.
const MAX_CONNECTION_TOKENS: usize = 16;

/// Returns the destination the request goes to, without touching the clock or the network.
///
/// Precedence is the contract:
///
/// 1. a destination a filter recorded — that filter owns the target kind the route named
///    (`gateway-ai` for a provider), and this crate must not second-guess it;
/// 2. the document upstream the route names, if the document has it;
/// 3. a refusal. A provider target with no filter is [`HttpError::ProviderNotConfigured`], because
///    this crate does not know provider semantics and guessing an address would be worse than
///    failing; an upstream name the document lacks is [`HttpError::UpstreamFailed`], which reference
///    validation makes unreachable for a published document.
///
/// # Errors
/// Returns the failure that explains why no destination exists. It is never a fallback: the request
/// ends with the response the error maps to.
pub(super) fn destination(ctx: &HttpContext) -> Result<Destination, HttpError> {
    if let Some(destination) = ctx.destination() {
        return Ok(destination.clone());
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

/// Returns the header names a request nominates in its `Connection` header.
///
/// RFC 9110 §7.6.1: a connection option lists headers that apply to this connection only, and a
/// proxy must remove them with the `Connection` header itself. Tokens are lowercased and
/// deduplicated by the caller's removal path, so the order here does not matter; a non-UTF-8 value
/// nominates nothing, because there is no name to remove.
///
/// # Errors
/// Returns [`HttpError::InvalidRequest`] when the header names more than [`MAX_CONNECTION_TOKENS`]
/// headers: the request is refused rather than forwarded with some of its nominated headers, which
/// would hand the upstream a connection semantic it must not inherit.
pub(super) fn connection_tokens(request: &RequestHeader) -> Result<Vec<String>, HttpError> {
    let Some(value) = request.headers.get("connection") else {
        return Ok(Vec::new());
    };
    let Ok(value) = value.to_str() else {
        return Ok(Vec::new());
    };

    let mut tokens = Vec::new();
    for token in value.split(',').map(str::trim) {
        if token.is_empty() {
            continue;
        }
        if tokens.len() == MAX_CONNECTION_TOKENS {
            return Err(HttpError::InvalidRequest {
                reason: format!(
                    "the `connection` header nominates more than {MAX_CONNECTION_TOKENS} headers"
                )
                .into(),
            });
        }
        tokens.push(token.to_ascii_lowercase());
    }
    Ok(tokens)
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
