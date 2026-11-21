//! The endpoint list of an etcd source: each endpoint's shape, and the one transport they share.
//!
//! `etcd-client` prefixes a bare `host:port`, honours a written `http://`, and connects an
//! `https://` endpoint only when TLS is configured for the whole client. So the scheme is not a
//! property of an address — it is a property of the client, and a list that disagrees with itself
//! or with the `tls` block is a startup error rather than a scheme guessed per endpoint.
//!
//! Shape and transport are two separate passes on purpose: a misspelt host and a missing `tls`
//! block are different mistakes, and the more basic one is the one reported.

use std::borrow::Cow;

use crate::bootstrap::BootstrapError;

/// Validates the endpoint list: at least one endpoint, each a usable shape, one transport for all.
///
/// `tls` is whether the section carried a `tls` block, and it decides the rest of the matrix: an
/// `https://` endpoint needs one, and a block may only speak to `https://` endpoints.
///
/// # Errors
/// Returns [`BootstrapError::InvalidField`] under `config_source.endpoints` when the list is empty or
/// mixes schemes, and under `config_source.endpoints[i]` when one endpoint is not a usable shape or
/// not reachable over the transport the operator stated.
pub(super) fn validate(endpoints: &[String], tls: bool) -> Result<(), BootstrapError> {
    if endpoints.is_empty() {
        return Err(BootstrapError::InvalidField {
            field: "config_source.endpoints".into(),
            reason: "must list at least one endpoint".into(),
        });
    }
    for (index, endpoint) in endpoints.iter().enumerate() {
        validate_endpoint(endpoint, index)?;
    }

    // One client serves the whole list over one transport, so a mixed list has no meaning: TLS is
    // configuration of the client, not of an address.
    let schemes: Vec<Scheme> = endpoints
        .iter()
        .map(|endpoint| Scheme::of(endpoint))
        .collect();
    if schemes.windows(2).any(|pair| pair[0] != pair[1]) {
        return Err(BootstrapError::InvalidField {
            field: "config_source.endpoints".into(),
            reason: "must use one scheme; every endpoint of one client shares its transport".into(),
        });
    }
    for (index, scheme) in schemes.into_iter().enumerate() {
        match (scheme, tls) {
            (Scheme::Https, false) => {
                return Err(endpoint_invalid(
                    index,
                    "is an https endpoint, which requires a `config_source.tls` block naming the \
                     trust root",
                ));
            }
            (Scheme::Plaintext, true) => {
                return Err(endpoint_invalid(
                    index,
                    "is a plaintext endpoint, but a `config_source.tls` block is present; write it \
                     as `https://` so the transport is stated rather than inferred",
                ));
            }
            (Scheme::Https, true) | (Scheme::Plaintext, false) => {}
        }
    }
    Ok(())
}

/// The transport an endpoint's scheme states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scheme {
    /// `https://`: TLS, and only with a `tls` block.
    Https,
    /// `http://` or a bare `host:port`: what `etcd-client` prefixes for a cleartext channel.
    Plaintext,
}

impl Scheme {
    /// Reads the scheme an endpoint declares.
    fn of(endpoint: &str) -> Self {
        if endpoint.starts_with("https://") {
            Self::Https
        } else {
            Self::Plaintext
        }
    }
}

/// Validates one endpoint's shape.
///
/// `etcd-client` accepts an `http://` endpoint as written, prefixes `http://` onto a bare
/// `host:port` (and `https://` once TLS is configured), so the *shape* is what can be checked here
/// and the scheme matrix above decides which of them a configured client can serve. The full URI
/// parse belongs to the client; a shape-bounded endpoint that survives this check fails at connect
/// only when the host is genuinely wrong.
fn validate_endpoint(endpoint: &str, index: usize) -> Result<(), BootstrapError> {
    if endpoint.trim() != endpoint {
        return Err(endpoint_invalid(
            index,
            "must not contain surrounding whitespace",
        ));
    }
    let authority = endpoint
        .strip_prefix("https://")
        .or_else(|| endpoint.strip_prefix("http://"))
        .unwrap_or(endpoint);
    // Everything after the first `/` is a path the etcd client would only serve the gRPC protocol
    // on; a declared path cannot be honoured, so the authority alone is what is validated.
    let authority = authority
        .split_once('/')
        .map_or(authority, |(authority, _)| authority);
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };

    if host.is_empty() || (host.contains(':') && !host.starts_with('[')) {
        return Err(endpoint_invalid(
            index,
            format!("has an unusable host `{host}`"),
        ));
    }
    if let Some(port) = port {
        let port: u16 = port.parse().map_err(|_| {
            endpoint_invalid(
                index,
                format!("has an unusable port `{port}`; it must be numeric"),
            )
        })?;
        if port == 0 {
            return Err(endpoint_invalid(index, "has port 0; ports start at 1"));
        }
    }
    Ok(())
}

/// Builds the rejection for one endpoint of the list.
fn endpoint_invalid(index: usize, reason: impl Into<Cow<'static, str>>) -> BootstrapError {
    BootstrapError::InvalidField {
        field: format!("config_source.endpoints[{index}]").into(),
        reason: reason.into(),
    }
}
