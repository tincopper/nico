//! The Pingora adapter: one method and one path in, one response out.
//!
//! This is the whole transport vocabulary. It does not know the endpoint names, the status codes
//! or the body shapes — it reads the request line, hands it to the capability layer with the two
//! facts that layer cannot read itself, and maps the returned [`Response`] onto the wire type.

use std::sync::Arc;

use async_trait::async_trait;
use nico_core::SnapshotHandle;
use nico_http::HttpStateHandle;
use nico_metrics::{Metrics, Readiness, Response as Reply};
use pingora::{apps::http_app::ServeHttp, protocols::http::ServerSession};

/// The self-reporting application: the recorder, the configuration-state handle and the value the
/// request path reads.
pub(crate) struct MetricsApp {
    pub(super) registry: Metrics,
    pub(super) snapshot: Arc<SnapshotHandle>,
    pub(super) state: Arc<HttpStateHandle>,
}

#[async_trait]
impl ServeHttp for MetricsApp {
    async fn response(&self, session: &mut ServerSession) -> http::Response<Vec<u8>> {
        let head = session.req_header();
        let method = head.method.as_str();
        let path = head.uri.path();
        // The status is cloned (it carries the digest), and the servability verdict comes from the
        // same handle the request path reads. The capability layer decides what the answer is.
        let status = self.snapshot.status();
        let reply =
            self.registry
                .respond(method, path, Readiness::new(self.state.servable(), &status));
        into_pingora_response(reply)
    }
}

/// Maps one capability response onto the wire type Pingora's `ServeHttp` returns.
///
/// This is the only place the mapping exists. The headers come from exactly three sources: the
/// content type the capability layer declared, the length of the body actually written, and the
/// `Allow` value it declared for a `405`. Nothing else is added, so the surface cannot accidentally
/// leak a header the capability layer never chose.
fn into_pingora_response(reply: Reply) -> http::Response<Vec<u8>> {
    let status = match http::StatusCode::from_u16(reply.status()) {
        Ok(status) => status,
        Err(error) => {
            // Every status the capability layer can produce is valid; a failure here is a defect
            // in that layer, and the connection still gets an answer.
            tracing::error!(
                status = reply.status(),
                error = %error,
                "the self-reporting status is not a valid HTTP status; answering 500"
            );
            http::StatusCode::INTERNAL_SERVER_ERROR
        }
    };
    let body = reply.body().as_bytes().to_vec();
    let mut builder = http::Response::builder()
        .status(status)
        .header("content-type", reply.content_type())
        .header("content-length", body.len());
    if let Some(allow) = reply.allow() {
        builder = builder.header("allow", allow);
    }
    // The status was validated and the header values are constants, so building cannot fail; a
    // failure would be a defect in this function, and the fallback still answers.
    builder.body(body).unwrap_or_default()
}
