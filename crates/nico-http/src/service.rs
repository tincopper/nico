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

use std::{sync::Arc, time::Duration};

use nico_core::MAX_ATTEMPTS_PER_ROUTE;
use pingora::{
    apps::HttpServerOptions,
    proxy::{HttpProxy, ProxyServiceBuilder},
    server::configuration::ServerConf,
    services::listening::Service,
};

use crate::{proxy::HttpApp, settings::HttpSettings, state::HttpStateHandle, upstream::tls};

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
#[must_use]
pub fn service(
    conf: &Arc<ServerConf>,
    state: Arc<HttpStateHandle>,
    settings: &HttpSettings,
) -> Service<HttpProxy<HttpApp, ()>> {
    let app = HttpApp::new(state);
    // `HttpServerOptions` is non-exhaustive, so it is defaulted first and then narrowed: `h2c` is
    // the one option this build decides, and `ProxyServiceBuilder` is the only path that can set
    // it at all (`http_proxy_service()` cannot).
    let mut options = HttpServerOptions::default();
    options.h2c = settings.h2c();
    let mut service = ProxyServiceBuilder::new(conf, app)
        .server_options(options)
        .build();
    service.add_tcp(settings.listen());
    service
}

/// Rounds a configured window up to whole seconds and never down to zero.
fn whole_seconds(window: Duration) -> u64 {
    let seconds = window.as_secs() + u64::from(window.subsec_nanos() > 0);
    seconds.max(1)
}

#[cfg(test)]
mod tests;
