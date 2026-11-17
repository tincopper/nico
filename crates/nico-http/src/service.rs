//! Process-level assembly of the HTTP data plane.
//!
//! Two things live here, and each of them is the only copy of its decision:
//!
//! - [`server_conf`] turns validated [`HttpSettings`] into Pingora's `ServerConf`, so a drain
//!   window, a thread count or the retry policy is never spelled out anywhere else.
//! - [`service`] builds the proxy service: one `HttpProxy` over [`HttpApp`], one listener.
//!
//! Neither touches the process: the runtime, the signals and the shutdown token belong to the
//! binary that assembles them (`nico`, built by the `nico-server` crate), which is what keeps this crate a servable
//! library — it can be embedded in a test or a second front-end without installing a signal
//! handler as a side effect of being linked.

use std::{io, sync::Arc, time::Duration};

use nico_core::MAX_ATTEMPTS_PER_ROUTE;
use pingora::{
    apps::HttpServerOptions,
    listeners::tls::TlsSettings,
    proxy::{HttpProxy, ProxyServiceBuilder},
    server::configuration::ServerConf,
    services::listening::Service,
};

use crate::{
    inbound::{
        InboundTls, StaticCertResolver, certified_key, read_identity, validate_server_identity,
    },
    proxy::HttpApp,
    settings::HttpSettings,
    state::HttpStateHandle,
    upstream::tls,
};

/// Builds the server configuration the data plane runs with.
///
/// Both drain windows are stored as whole seconds because that is what Pingora reads, and they are
/// rounded **up**: a configured window is a lower bound on how long a response may keep draining,
/// so truncating `1500 ms` to one second would cut a stream short. The result is never zero — zero
/// would mean no grace period at all — and it is always `Some`, because `None` would fall back to
/// Pingora's own 300 s / 5 s defaults and silently ignore what the bootstrap file configured.
///
/// `max_retries` is Pingora's number of *attempts*, not the number of extra tries: its proxy loop
/// is `while retries < max_retries`, so `N` means at most `N` upstream requests. `0` would skip the
/// loop entirely and answer nothing.
///
/// The value is the **ceiling** of a route's attempt sequence, not the sequence itself: a route
/// declares its own `retry_budget` and `fallback` chain, and the walk stops asking for another
/// attempt as soon as those are spent. The framework's counter is the loop's fuse, and it is sized by
/// the document model rather than guessed here: [`MAX_ATTEMPTS_PER_ROUTE`] is the product of the two
/// bounds a document is validated against, so no legal route can outrun the fuse and no edit to one
/// bound can leave the other behind. A route that declared nothing still makes exactly one attempt,
/// because its walk answers "no" immediately.
///
/// `threads` is never zero — [`HttpSettings`] rejects zero for the same reason Pingora would panic
/// on it, so the value handed to Tokio's `worker_threads` is always a usable runtime size.
///
/// The upstream trust roots are written here because this is the last moment before Pingora reads
/// them: `ProxyServiceBuilder::build` derives its TLS connector from this value, once. That is also
/// why changing the CA is a restart and not a reload.
#[must_use]
pub fn server_conf(settings: &HttpSettings) -> Arc<ServerConf> {
    let mut conf = ServerConf {
        grace_period_seconds: Some(whole_seconds(settings.grace_period())),
        graceful_shutdown_timeout_seconds: Some(whole_seconds(settings.graceful_shutdown())),
        threads: settings.threads(),
        max_retries: MAX_ATTEMPTS_PER_ROUTE,
        ..ServerConf::default()
    };
    tls::apply(settings.tls(), &mut conf);
    Arc::new(conf)
}

/// Builds the proxy service and registers the listener the settings name.
///
/// `state` is the only configuration source the request path reads: the reload pipeline publishes a
/// snapshot and the plan compiled from it together, so the data plane never pairs one generation's
/// routes with another's policies.
///
/// A configured `http.tls` makes this a TLS listener: the certificate and key are read and checked
/// again here (the bootstrap already did it once), the resulting identity is handed to Pingora as a
/// static resolver, and ALPN advertises `h2`/`http/1.1`. The resolver is deliberate: Pingora's
/// path-based settings load the files during `build()` and panic on a bad or changed pair, whereas a
/// resolver only uses the bytes this function already validated.
///
/// # Errors
/// Returns [`ServiceBuildError`] when the listener's TLS identity cannot be read or validated, or
/// when Pingora rejects the TLS settings. A failed build leaves the process unstarted; it never
/// serves a listener whose certificate was silently ignored.
pub fn service(
    conf: &Arc<ServerConf>,
    state: Arc<HttpStateHandle>,
    settings: &HttpSettings,
) -> Result<Service<HttpProxy<HttpApp, ()>>, ServiceBuildError> {
    let mut app = HttpApp::new(state);
    app.set_inbound_tls(settings.inbound_tls().is_some());
    // `HttpServerOptions` is non-exhaustive, so it is defaulted first and then narrowed: `h2c` is
    // the one option this build decides, and `ProxyServiceBuilder` is the only path that can set
    // it at all (`http_proxy_service()` cannot).
    let mut options = HttpServerOptions::default();
    options.h2c = settings.h2c();
    let mut service = ProxyServiceBuilder::new(conf, app)
        .server_options(options)
        .build();

    if let Some(inbound) = settings.inbound_tls() {
        let tls = tls_settings(inbound)?;
        service.add_tls_with_settings(settings.listen(), None, tls);
    } else {
        service.add_tcp(settings.listen());
    }
    Ok(service)
}

/// Why a service with a configured listener could not be built.
///
/// Every variant is a startup failure. The messages name the file or the rule; no certificate or key
/// material is ever part of one.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ServiceBuildError {
    /// The certificate file could not be read.
    #[error("the inbound TLS certificate cannot be read: {source}")]
    CertificateRead {
        /// The file-system error.
        #[source]
        source: io::Error,
    },
    /// The private-key file could not be read.
    #[error("the inbound TLS private key cannot be read: {source}")]
    KeyRead {
        /// The file-system error.
        #[source]
        source: io::Error,
    },
    /// The certificate and key are not a usable identity.
    #[error("the inbound TLS identity is invalid: {source}")]
    Identity {
        /// The validation error.
        #[source]
        source: crate::inbound::ServerIdentityError,
    },
    /// One of the configured paths cannot be written as a Pingora TLS setting.
    #[error("the inbound TLS path is not valid UTF-8")]
    Path,
    /// Pingora rejected the TLS settings.
    #[error("the inbound TLS settings could not be built: {source}")]
    Tls {
        /// Pingora's diagnostic.
        #[source]
        source: pingora::Error,
    },
}

/// Builds Pingora's TLS listener settings from a validated identity.
fn tls_settings(inbound: &InboundTls) -> Result<TlsSettings, ServiceBuildError> {
    let (certificate, key) = read_identity(inbound).map_err(|error| match error {
        crate::inbound::IdentityReadError::Certificate { source } => {
            ServiceBuildError::CertificateRead { source }
        }
        crate::inbound::IdentityReadError::Key { source } => ServiceBuildError::KeyRead { source },
    })?;
    validate_server_identity(&certificate, &key)
        .map_err(|source| ServiceBuildError::Identity { source })?;
    let identity = certified_key(&certificate, &key)
        .map_err(|source| ServiceBuildError::Identity { source })?;

    let cert_path = inbound
        .cert_file()
        .to_str()
        .ok_or(ServiceBuildError::Path)?;
    let key_path = inbound.key_file().to_str().ok_or(ServiceBuildError::Path)?;
    let mut settings = TlsSettings::intermediate(cert_path, key_path)
        .map_err(|source| ServiceBuildError::Tls { source: *source })?;
    settings.set_cert_resolver(Arc::new(StaticCertResolver::new(identity)));
    settings.enable_h2();
    Ok(settings)
}

/// Rounds a configured window up to whole seconds and never down to zero.
fn whole_seconds(window: Duration) -> u64 {
    let seconds = window.as_secs() + u64::from(window.subsec_nanos() > 0);
    seconds.max(1)
}

#[cfg(test)]
mod tests;
