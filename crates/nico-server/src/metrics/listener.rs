//! The listener assembly: one Pingora service, one address.

use std::sync::Arc;

use nico_core::SnapshotHandle;
use nico_http::HttpStateHandle;
use nico_metrics::Metrics;
use pingora::{apps::http_app::HttpServer, services::listening::Service};

use super::app::MetricsApp;
use crate::bootstrap::MetricsSettings;

/// Builds the self-reporting service and registers the listener the settings name.
///
/// The service is deliberately separate from the data plane's: one Pingora service carries one
/// application, and sharing the data plane's would route `/metrics` through the full proxy hook
/// chain, making the surface a route a pushed document could shadow.
pub(crate) fn service(
    settings: &MetricsSettings,
    registry: Metrics,
    snapshot: Arc<SnapshotHandle>,
    state: Arc<HttpStateHandle>,
) -> Service<HttpServer<MetricsApp>> {
    let app = HttpServer::new_app(MetricsApp {
        registry,
        snapshot,
        state,
    });
    let mut service = Service::new("metrics".to_string(), app);
    service.add_tcp(settings.listen());
    service
}
