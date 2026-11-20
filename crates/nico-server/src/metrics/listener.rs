//! The listener assembly: one Pingora service, one address.
//!
//! The transport is a property of the section, not a second code path: a `tls` block turns this
//! socket into a rustls endpoint through [`nico_http::listener_tls`], the same builder the data
//! plane's TLS entries use. Nothing about ALPN, protocol versions or the certificate resolver is
//! decided here — this module only says which of the two transports the file asked for.

use std::sync::Arc;

use nico_core::SnapshotHandle;
use nico_http::{HttpStateHandle, ListenerAlpn, ServiceBuildError, listener_tls};
use nico_metrics::Metrics;
use pingora::{apps::http_app::HttpServer, services::listening::Service};

use super::app::MetricsApp;
use crate::bootstrap::MetricsSettings;

/// Builds the self-reporting service and registers the listener the settings name.
///
/// The service is deliberately separate from the data plane's: one Pingora service carries one
/// application, and sharing the data plane's would route `/metrics` through the full proxy hook
/// chain, making the surface a route a pushed document could shadow.
///
/// ALPN offers `http/1.1` only. Scrapers and probes speak nothing else, and a second protocol would
/// be a second untested path on the one surface whose job is to answer reliably — while the data
/// plane keeps offering `h2` and `http/1.1` on its own listeners.
///
/// # Errors
/// Returns [`ServiceBuildError`] when the settings name a `tls` block whose identity cannot be read,
/// validated or turned into TLS settings. The bootstrap already read and paired the same files; this
/// second read is what turns a file that changed in between into a startup error instead of a panic
/// inside Pingora's loader. A caller must refuse to start.
pub(crate) fn service(
    settings: &MetricsSettings,
    registry: Metrics,
    snapshot: Arc<SnapshotHandle>,
    state: Arc<HttpStateHandle>,
) -> Result<Service<HttpServer<MetricsApp>>, ServiceBuildError> {
    let app = HttpServer::new_app(MetricsApp {
        registry,
        snapshot,
        state,
    });
    let mut service = Service::new("metrics".to_string(), app);
    match settings.tls() {
        Some(identity) => {
            let tls = listener_tls(identity, ListenerAlpn::Http1)?;
            service.add_tls_with_settings(settings.listen(), None, tls);
        }
        None => service.add_tcp(settings.listen()),
    }
    Ok(service)
}
