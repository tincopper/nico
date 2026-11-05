//! The one-publication invariant for a document that declares plugins.
//!
//! A generation is the snapshot an operator reads plus everything the request path reads with it: the
//! global chain, the per-route chains and the routes that buffer their body. They are only safe
//! together — a snapshot whose route table is newer than its policies would serve a policy the operator
//! did not publish, and for an authentication plugin that is an open door.
//!
//! These tests are on a paused clock and drive the pipeline directly, so "the publish was refused" is
//! an observed state rather than a race: nothing else can move while the test advances time.

mod support;

use std::{path::Path, sync::Arc};

use nico_config::{ConfigSource, ConfigVersion};
use nico_core::Revision;

use crate::support::{ChannelSource, Harness, token};

/// The secret the fixture JWT is signed with.
const SECRET: &str = "a-shared-secret-used-by-the-publication-scenario";

/// Builds a document whose `api` route references one `jwt` plugin.
///
/// `body_limit` differs between documents so a test can read "which document is in service" off a
/// value that has nothing to do with plugins.
fn document(secret_ref: &str, body_limit: u32) -> bytes::Bytes {
    format!(
        "\
apiVersion: gateway/v1
request:
  max_body_bytes: {body_limit}
  timeout_ms: 30000
providers: []
upstreams:
  - name: api-backend
    scheme: http
    host: 127.0.0.1
    port: 8080
plugins:
  - name: public-jwt
    kind: jwt
    buffers_body: true
    config:
      keys:
        - kid: k1
          secret_ref: {secret_ref}
# The global use site: the same instance runs before routing and on the route that names it.
global_plugins: [public-jwt]
routes:
  - id: api
    path_prefix: /api/
    upstream: api-backend
    plugins: [public-jwt]
"
    )
    .into()
}

/// Returns the generation the request path would serve, or `None` before a publish.
fn served_generation(harness: &Harness) -> Option<u64> {
    let state = harness.state().current()?;
    Some(state.snapshot().generation().get())
}

/// Reports whether the published value carries everything the fixture document compiled to.
///
/// The three parts are read from one `current()`, which is the property being asserted: a generation
/// whose route table is current but whose global chain or body mode came from another publication would
/// run the wrong policies. A snapshot-only assertion would miss exactly that.
fn plan_published(harness: &Harness) -> bool {
    harness.state().current().is_some_and(|state| {
        state.chains().chain_for("api").is_some()
            // The chain prints instance names, which is what a log attributes a decision to: the
            // document named this entry `public-jwt`, not `jwt`.
            && format!("{:?}", state.global()) == r#"["public-jwt"]"#
            && state.buffered().contains("api")
    })
}

#[tokio::test(flavor = "current_thread")]
async fn a_refused_plugin_document_keeps_the_published_pair_in_service() {
    tokio::time::pause();
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let secret = token::secret_file(directory.path(), "jwt.secret", SECRET);
    let secret_path = secret
        .strip_prefix("file:")
        .map(Path::new)
        .expect("the fixture reference names a file")
        .to_path_buf();

    let source = ChannelSource::new("channel");
    source.push_load(Ok(ConfigVersion::new(
        Revision::comparable(1),
        document(&secret, 1024),
    )));
    let harness = Harness::start(Arc::clone(&source) as Arc<dyn ConfigSource>);

    let status = harness.advance_until(Harness::ready).await;
    assert_eq!(status.generation, 1);
    assert!(!status.degraded, "a compiled document is not degraded");
    assert_eq!(
        served_generation(&harness),
        Some(1),
        "the request path serves the generation the status reports"
    );
    assert!(
        plan_published(&harness),
        "the route's chain, the global chain and the body mode are published"
    );

    // The credential disappears and the document changes. Compiling the plan now fails, so the
    // document is refused: both readers keep the *previous* pair, and the pipeline records why.
    std::fs::remove_file(&secret_path).expect("the credential fixture is removed");
    source.push_comparable(2, &document(&secret, 2048)).await;
    let status = harness
        .advance_until(|harness| harness.status().degraded)
        .await;

    assert_eq!(
        status.generation, 1,
        "a refused document does not advance the generation"
    );
    assert_eq!(
        harness.body_limit(),
        Some(1024),
        "the document in service is still the one that compiled"
    );
    assert_eq!(
        served_generation(&harness),
        Some(1),
        "and the request path still reads it"
    );
    assert!(
        plan_published(&harness),
        "a refused publication must not leave the plan half-written"
    );

    // Restoring the credential makes the next document compilable, and both readers advance together.
    std::fs::write(&secret_path, SECRET).expect("the credential fixture is restored");
    source.push_comparable(3, &document(&secret, 4096)).await;
    let status = harness
        .advance_until(|harness| harness.generation() == 2)
        .await;

    assert_eq!(status.generation, 2);
    assert!(!status.degraded, "a later success clears the degraded flag");
    assert_eq!(harness.body_limit(), Some(4096));
    assert_eq!(
        served_generation(&harness),
        Some(2),
        "the two readers report the same generation"
    );
    assert!(plan_published(&harness), "the whole plan moved with it");

    harness.stop().await.expect("shutdown ends the pipeline");
}
