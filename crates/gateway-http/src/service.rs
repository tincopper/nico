//! Process-level assembly of the HTTP data plane.
//!
//! Two things live here, and each of them is the only copy of its decision:
//!
//! - [`server_conf`] turns validated [`HttpSettings`] into Pingora's `ServerConf`, so a drain
//!   window, a thread count or the retry policy is never spelled out anywhere else.
//! - [`service`] builds the proxy service: one `HttpProxy` over [`HttpApp`], one listener.
//!
//! Neither touches the process: the runtime, the signals and the shutdown token belong to the
//! binary that assembles them (`gateway-server`), which is what keeps this crate a servable
//! library — it can be embedded in a test or a second front-end without installing a signal
//! handler as a side effect of being linked.

use std::{sync::Arc, time::Duration};

use gateway_core::SnapshotHandle;
use pingora::{
    apps::HttpServerOptions,
    proxy::{HttpProxy, ProxyServiceBuilder},
    server::configuration::ServerConf,
    services::listening::Service,
};

use crate::{filter::FilterChain, proxy::HttpApp, settings::HttpSettings};

/// Builds the server configuration the data plane runs with.
///
/// Both drain windows are stored as whole seconds because that is what Pingora reads, and they are
/// rounded **up**: a configured window is a lower bound on how long a response may keep draining,
/// so truncating `1500 ms` to one second would cut a stream short. The result is never zero — zero
/// would mean no grace period at all — and it is always `Some`, because `None` would fall back to
/// Pingora's own 300 s / 5 s defaults and silently ignore what the bootstrap file configured.
///
/// `max_retries` is Pingora's number of *attempts*, not the number of extra tries: its proxy loop
/// is `while retries < max_retries`, so `1` means exactly one upstream request and no retry. `0`
/// would skip the loop entirely and answer nothing.
///
/// `threads` is never zero — [`HttpSettings`] rejects zero for the same reason Pingora would panic
/// on it, so the value handed to Tokio's `worker_threads` is always a usable runtime size.
#[must_use]
pub fn server_conf(settings: &HttpSettings) -> Arc<ServerConf> {
    let conf = ServerConf {
        grace_period_seconds: Some(whole_seconds(settings.grace_period())),
        graceful_shutdown_timeout_seconds: Some(whole_seconds(settings.graceful_shutdown())),
        threads: settings.threads(),
        max_retries: 1,
        ..ServerConf::default()
    };
    Arc::new(conf)
}

/// Builds the proxy service and registers the listener the settings name.
#[must_use]
pub fn service(
    conf: &Arc<ServerConf>,
    snapshot: Arc<SnapshotHandle>,
    filters: FilterChain,
    settings: &HttpSettings,
) -> Service<HttpProxy<HttpApp, ()>> {
    let app = HttpApp::new(snapshot, filters);
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
