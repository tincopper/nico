//! Process-level assembly of the HTTP data plane.
//!
//! Three things live here, and each of them is the only copy of its decision:
//!
//! - [`server_conf`] turns the validated runtime settings and the upstream trust roots into Pingora's
//!   `ServerConf`, so a drain window, a thread count or the retry policy is never spelled out
//!   anywhere else.
//! - [`service`] builds the proxy service: one `HttpProxy` over [`HttpApp`], and every listener the
//!   settings name — plaintext entries as TCP, TLS entries as rustls endpoints.
//! - [`listener_tls`] is what a TLS listener *is*: the identity read, paired and handed to rustls as
//!   a static resolver, plus the ALPN offer the caller picked. The self-reporting surface terminates
//!   TLS through the same function, so protocol versions and resolver policy have one definition.
//!
//! None of them touches the process: the runtime, the signals and the shutdown token belong to the
//! binary that assembles them (`nico`, built by the `nico-server` crate), which is what keeps this crate a servable
//! library — it can be embedded in a test or a second front-end without installing a signal
//! handler as a side effect of being linked.

use std::{io, sync::Arc};

use nico_core::MAX_ATTEMPTS_PER_ROUTE;
use pingora::{
    apps::HttpServerOptions,
    listeners::tls::TlsSettings,
    protocols::tls::ALPN,
    proxy::{HttpProxy, ProxyServiceBuilder},
    server::configuration::ServerConf,
    services::listening::Service,
};

use crate::{
    inbound::{
        InboundTls, StaticCertResolver, certified_key, read_identity, validate_server_identity,
    },
    proxy::HttpApp,
    server_settings::ServerSettings,
    settings::{HttpSettings, Listener},
    state::HttpStateHandle,
    upstream::{tls, tls::TlsPolicy},
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
/// `threads` is never zero — [`ServerSettings`] rejects zero for the same reason Pingora would panic
/// on it, so the value handed to Tokio's `worker_threads` is always a usable runtime size.
///
/// The upstream trust roots are written here because this is the last moment before Pingora reads
/// them: `ProxyServiceBuilder::build` derives its TLS connector from this value, once. That is also
/// why changing the CA is a restart and not a reload.
#[must_use]
pub fn server_conf(runtime: &ServerSettings, tls: &TlsPolicy) -> Arc<ServerConf> {
    let mut conf = ServerConf {
        grace_period_seconds: Some(whole_seconds(runtime.grace_period_ms())),
        graceful_shutdown_timeout_seconds: Some(whole_seconds(runtime.graceful_shutdown_ms())),
        threads: runtime.threads(),
        max_retries: MAX_ATTEMPTS_PER_ROUTE,
        ..ServerConf::default()
    };
    tls::apply(tls, &mut conf);
    Arc::new(conf)
}

/// Builds the proxy service and registers every listener the settings name.
///
/// `state` is the only configuration source the request path reads: the reload pipeline publishes a
/// snapshot and the plan compiled from it together, so the data plane never pairs one generation's
/// routes with another's policies.
///
/// Each plaintext entry is registered as TCP. Each TLS entry re-reads and re-checks its certificate
/// and key here (the bootstrap already did it once), hands the resulting identity to Pingora as a
/// static resolver, and advertises `h2`/`http/1.1` over ALPN. The resolver is deliberate: Pingora's
/// path-based settings load the files during `build()` and panic on a bad or changed pair, whereas a
/// resolver only uses the bytes this function already validated. A plaintext and a TLS listener can
/// coexist — they are entries of the same section, each serving its own transport.
///
/// The listeners are non-empty and pairwise distinct by [`HttpSettings::resolve`], so the section
/// the settings came from cannot produce a service with nothing to bind or two listeners fighting
/// over one socket.
///
/// # Errors
/// Returns [`ServiceBuildError`] when a listener's TLS identity cannot be read or validated, or
/// when Pingora rejects the TLS settings. A failed build leaves the process unstarted; it never
/// serves a listener whose certificate was silently ignored.
pub fn service(
    conf: &Arc<ServerConf>,
    state: Arc<HttpStateHandle>,
    settings: &HttpSettings,
) -> Result<Service<HttpProxy<HttpApp, ()>>, ServiceBuildError> {
    // `HttpServerOptions` is non-exhaustive, so it is defaulted first and then narrowed: `h2c` is
    // the one option this build decides, and `ProxyServiceBuilder` is the only path that can set
    // it at all (`http_proxy_service()` cannot). It applies to the plaintext listeners; a TLS
    // listener negotiates HTTP/2 through ALPN and is not affected.
    let mut options = HttpServerOptions::default();
    options.h2c = settings.h2c();
    let mut service = ProxyServiceBuilder::new(conf, HttpApp::new(state))
        .server_options(options)
        .build();

    for listener in settings.listeners() {
        match listener {
            Listener::Plain { listen } => service.add_tcp(listen),
            Listener::Tls { listen, tls } => {
                let tls = listener_tls(tls, ListenerAlpn::Http2Or1)?;
                service.add_tls_with_settings(listen, None, tls);
            }
        }
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

/// Which protocols a TLS listener offers over ALPN.
///
/// The choice is the listener's, not the process's: the data plane wants a second protocol on the
/// wire, and the self-reporting surface does not. A boolean would be the wrong shape — `enable_h2()`
/// (offer `h2` and `http/1.1`) and "declare no ALPN at all" are different results, so one flag
/// could only express the second as "the first, with something switched off", which no test could
/// tell apart from forgetting to declare anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenerAlpn {
    /// HTTP/1.1 only — the self-reporting surface, whose clients are scrapers and probes.
    Http1,
    /// HTTP/2 preferred, HTTP/1.1 allowed — the data plane.
    Http2Or1,
}

/// Builds Pingora's TLS listener settings from an already-configured inbound identity.
///
/// This is the one definition of the inbound TLS policy — protocol versions, the static resolver,
/// the ALPN offer — and both listeners of a process go through it: the data plane's TLS entries and
/// the self-reporting surface. A caller decides *whether* a listener terminates TLS and *which*
/// protocols it offers; it never assembles TLS parameters of its own.
///
/// The files are read and paired again here even though the bootstrap already did it, because
/// Pingora's path-based settings would load them during `build()` and panic on a pair that changed
/// in between. Handing rustls a resolver built from these bytes means `build()` performs no file
/// I/O, and a post-check change is this error instead of an abort inside the framework.
///
/// The returned value is a Pingora type because it is Pingora that consumes it; the caller hands it
/// straight to `add_tls_with_settings` and needs no framework type in its own signatures.
///
/// # Errors
/// Returns [`ServiceBuildError`] when a file cannot be read, when the two are not a usable identity,
/// when a path is not UTF-8, or when Pingora rejects the TLS settings. A caller must treat every
/// variant as a startup failure; no listener may serve with the identity silently ignored.
pub fn listener_tls(
    identity: &InboundTls,
    alpn: ListenerAlpn,
) -> Result<TlsSettings, ServiceBuildError> {
    let (certificate, key) = read_identity(identity).map_err(|error| match error {
        crate::inbound::IdentityReadError::Certificate { source } => {
            ServiceBuildError::CertificateRead { source }
        }
        crate::inbound::IdentityReadError::Key { source } => ServiceBuildError::KeyRead { source },
    })?;
    validate_server_identity(&certificate, &key)
        .map_err(|source| ServiceBuildError::Identity { source })?;
    let certified = certified_key(&certificate, &key)
        .map_err(|source| ServiceBuildError::Identity { source })?;

    let cert_path = identity
        .cert_file()
        .to_str()
        .ok_or(ServiceBuildError::Path)?;
    let key_path = identity
        .key_file()
        .to_str()
        .ok_or(ServiceBuildError::Path)?;
    let mut settings = TlsSettings::intermediate(cert_path, key_path)
        .map_err(|source| ServiceBuildError::Tls { source: *source })?;
    settings.set_cert_resolver(Arc::new(StaticCertResolver::new(certified)));
    match alpn {
        ListenerAlpn::Http1 => settings.set_alpn(ALPN::H1),
        ListenerAlpn::Http2Or1 => settings.enable_h2(),
    }
    Ok(settings)
}

/// Rounds a configured window in milliseconds up to whole seconds and never down to zero.
///
/// The windows come from [`ServerSettings`], which rejects anything below one millisecond, so the
/// ceiling is at least one second: Pingora reads zero as "do not wait at all".
fn whole_seconds(window_ms: u64) -> u64 {
    window_ms.div_ceil(1000)
}

#[cfg(test)]
mod tests;
