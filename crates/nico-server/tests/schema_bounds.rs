//! The `schema_bounds` bootstrap section reaches the pipeline.
//!
//! Two things are proven here, and neither can be proven in a unit test of `bootstrap.rs`: that the
//! bound written in the local file is the bound `Document::parse` applies, and that a document which
//! breaks it is rejected like any other bad document — the last-known-good snapshot keeps serving and
//! the generation does not move.
//!
//! The clock is paused (`tokio::time::pause`) and driven by [`Harness::advance_until`], so a debounce
//! window costs no wall-clock time and "nothing else happened" is provable rather than a race.

mod support;

use std::{path::Path, sync::Arc};

use nico_config::{ConfigSource, ConfigVersion, FakeConfigSource};
use nico_core::{ErrorCategory, Revision};
use nico_server::{BootstrapConfig, ReloadError, serve};
use tokio_util::sync::CancellationToken;

use crate::support::{BootstrapFixture, ChannelSource, Harness, bootstrap};

/// Largest document these fixtures allow themselves; the route documents below are larger than the
/// paused-clock default in [`support`].
const DOCUMENT_MAX_BYTES: usize = 64 * 1024;

/// A document with one route, so a `max_routes: 1` bootstrap accepts it.
const ONE_ROUTE_A: &str = concat!(
    "apiVersion: gateway/v1\n",
    "request:\n",
    "  max_body_bytes: 1024\n",
    "  timeout_ms: 1000\n",
    "plugins: []\n",
    "providers: []\n",
    "upstreams:\n",
    "  - name: api-backend\n",
    "    scheme: http\n",
    "    host: 127.0.0.1\n",
    "    port: 8080\n",
    "routes:\n",
    "  - id: api-public-v1\n",
    "    path_prefix: /api/\n",
    "    upstream: api-backend\n",
);

/// A second one-route document, so the recovery publishes a new digest rather than repeating
/// [`ONE_ROUTE_A`].
const ONE_ROUTE_B: &str = concat!(
    "apiVersion: gateway/v1\n",
    "request:\n",
    "  max_body_bytes: 1024\n",
    "  timeout_ms: 1000\n",
    "plugins: []\n",
    "providers: []\n",
    "upstreams:\n",
    "  - name: api-backend\n",
    "    scheme: http\n",
    "    host: 127.0.0.1\n",
    "    port: 8080\n",
    "routes:\n",
    "  - id: api-public-v2\n",
    "    path_prefix: /api/v2/\n",
    "    upstream: api-backend\n",
);

/// A document with two routes, which a `max_routes: 1` bootstrap must reject.
const TWO_ROUTES: &str = concat!(
    "apiVersion: gateway/v1\n",
    "request:\n",
    "  max_body_bytes: 1024\n",
    "  timeout_ms: 1000\n",
    "plugins: []\n",
    "providers: []\n",
    "upstreams:\n",
    "  - name: api-backend\n",
    "    scheme: http\n",
    "    host: 127.0.0.1\n",
    "    port: 8080\n",
    "routes:\n",
    "  - id: api-public-v1\n",
    "    path_prefix: /api/\n",
    "    upstream: api-backend\n",
    "  - id: api-public-v2\n",
    "    path_prefix: /api/v2/\n",
    "    upstream: api-backend\n",
);

/// Builds a bootstrap whose `config_source.schema_bounds` block holds `bounds`.
fn fixture(directory: &Path, bounds: &str) -> BootstrapConfig {
    bootstrap(
        directory,
        BootstrapFixture {
            max_bytes: DOCUMENT_MAX_BYTES,
            schema_bounds: Some(bounds),
            ..BootstrapFixture::default()
        },
    )
}

#[tokio::test(flavor = "current_thread")]
async fn serve_rejects_a_document_above_the_bootstrap_route_bound() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let bootstrap = fixture(directory.path(), "    max_routes: 1\n");
    let source = FakeConfigSource::new("fake")
        .push_load(Ok(FakeConfigSource::version_at(1, TWO_ROUTES.as_bytes())));

    let error = serve(bootstrap, Arc::new(source), CancellationToken::new())
        .await
        .expect_err("two routes exceed a bound of one");

    match error {
        ReloadError::NotReady { category } => assert_eq!(category, ErrorCategory::Limit),
        other => panic!("expected NotReady, got {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_rejected_document_keeps_the_last_good_snapshot_and_the_generation() {
    tokio::time::pause();
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let bootstrap = fixture(directory.path(), "    max_routes: 1\n");
    let source = ChannelSource::new("channel");
    source.push_load(Ok(ConfigVersion::new(
        Revision::comparable(1),
        ONE_ROUTE_A.as_bytes().to_vec().into(),
    )));
    let harness = Harness::from_bootstrap(&bootstrap, Arc::clone(&source) as Arc<dyn ConfigSource>);

    let status = harness.advance_until(Harness::ready).await;
    assert_eq!(status.generation, 1);
    assert_eq!(harness.route_count(), Some(1));

    source.push_comparable(2, TWO_ROUTES.as_bytes()).await;
    let status = harness
        .advance_until(|harness| harness.status().degraded)
        .await;
    assert_eq!(status.generation, 1, "a rejected document must not publish");
    assert_eq!(status.last_error_category, Some(ErrorCategory::Limit));
    assert_eq!(harness.route_count(), Some(1));

    source.push_comparable(3, ONE_ROUTE_B.as_bytes()).await;
    let status = harness
        .advance_until(|harness| harness.generation() == 2 && !harness.status().degraded)
        .await;
    assert_eq!(status.last_error_category, None);
    assert_eq!(harness.route_count(), Some(1));

    harness.stop().await.expect("shutdown ends the pipeline");
}
