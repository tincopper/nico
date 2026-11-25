//! Prepares one selected upstream request: path, automatic forwarding headers, route overrides and framing.

use pingora::{http::RequestHeader, protocols::Digest, proxy::Session};

use super::{
    super::{
        HttpApp,
        failure::abort,
        forward::{forwarded_for, peer_ip, set_header},
    },
    llm,
};
use crate::{context::HttpContext, error::HttpError, upstream};

/// Rewrites the request headers for the upstream, and frames a buffered body as chunked.
///
/// What this hook no longer does is strip hop-by-hop headers: Pingora's
/// `HttpUpstreamRequestPolicy` removes the standard set and the ones a `Connection` header nominates.
pub(super) async fn upstream_request_filter(
    _app: &HttpApp,
    session: &mut Session,
    request: &mut RequestHeader,
    ctx: &mut HttpContext,
) -> pingora::Result<()> {
    if ctx.converting().is_some() {
        ctx.begin_attempt_body();
    }
    let client_addr = peer_ip(session);
    llm::apply_upstream_request(request, ctx).map_err(|error| abort(ctx, error))?;

    if let Some((prefix, replacement)) = ctx.rewrite() {
        let original = ctx.path().unwrap_or_default();
        let remainder = original.strip_prefix(prefix).unwrap_or(original);
        let target = match request.uri.query() {
            Some(query) => format!("{replacement}{remainder}?{query}"),
            None => format!("{replacement}{remainder}"),
        };
        request.set_raw_path(target.as_bytes()).map_err(|error| {
            abort(
                ctx,
                HttpError::Internal {
                    source: Box::new(error),
                },
            )
        })?;
    }

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
    let original = ctx.original_host().map(str::to_owned).or_else(|| {
        session
            .req_header()
            .headers
            .get("host")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    });
    if let Some(original) = original {
        set_header(request, "x-forwarded-host", &original, ctx)?;
    }
    if let Some(forwarded_for) = forwarded_for(request, client_addr) {
        set_header(request, "x-forwarded-for", &forwarded_for, ctx)?;
    }
    if request.headers.get("x-forwarded-proto").is_none() {
        let scheme = if is_tls_connection(session.digest()) {
            "https"
        } else {
            "http"
        };
        set_header(request, "x-forwarded-proto", scheme, ctx)?;
    }
    set_header(request, "host", &host, ctx)?;
    apply_proxy_headers(request, ctx)?;

    if ctx.buffers_body() || ctx.converting().is_some() {
        request.remove_header("content-length");
        set_header(request, "transfer-encoding", "chunked", ctx)?;
    }
    Ok(())
}

/// Applies the route's request-header rewrites, in document order.
fn apply_proxy_headers(request: &mut RequestHeader, ctx: &mut HttpContext) -> pingora::Result<()> {
    for name in ctx.proxy_del_header() {
        request.remove_header(name);
    }
    let entries: Vec<(String, String)> = ctx
        .proxy_set_header()
        .iter()
        .map(|entry| {
            let value = match entry.value() {
                nico_core::ProxyHeaderValue::Literal(literal) => literal.clone(),
                nico_core::ProxyHeaderValue::ClientHost => {
                    ctx.original_host().unwrap_or_default().to_owned()
                }
                nico_core::ProxyHeaderValue::UpstreamHost => {
                    ctx.upstream().unwrap_or_default().to_owned()
                }
            };
            (entry.name().to_owned(), value)
        })
        .collect();
    for (name, value) in entries {
        if value.is_empty() {
            request.remove_header(&name);
        } else {
            request
                .insert_header(name.clone(), &value)
                .map_err(|error| abort(ctx, HttpError::Internal { source: error }))?;
        }
    }
    Ok(())
}

/// Reports whether the downstream connection terminated TLS, from its own digest.
fn is_tls_connection(digest: Option<&Digest>) -> bool {
    digest
        .and_then(|digest| digest.ssl_digest.as_ref())
        .is_some()
}

#[cfg(test)]
mod tests;
