//! Publishing an AI route: the plan moves with the snapshot it was compiled from, and a refused
//! plan keeps the generation in service.
//!
//! The provider route is the one thing this file is about, so the document carries an upstream route
//! as well: it names a body limit that identifies which document is in service without reading
//! anything the LLM plan owns. The credential is a `file:` fixture — an `env:` reference could not be
//! established from a test (setting one is `unsafe` in the 2024 edition), and a missing variable is
//! exactly what the refused-document case must not be about here.
//!
//! The clock is paused, so "the publish was refused" is an observed state rather than a race.

mod support;

use std::{path::Path, sync::Arc};

use nico_config::{ConfigSource, ConfigVersion};
use nico_core::Revision;

use crate::support::{ChannelSource, Harness, token};

/// The provider key the fixture document references.
const KEY: &str = "sk-fixture-provider-key";

/// The address the published front-end must compile from the document's base URL.
const PROVIDER_ADDRESS: &str = "http://127.0.0.1:8080";

/// Builds a document with one provider route and one upstream route.
///
/// `protocol` is the only variable the tests change: an unknown one refuses the publication, a known
/// one compiles it.
fn document(credential_ref: &str, protocol: &str, body_limit: u32) -> bytes::Bytes {
    format!(
        "\
apiVersion: gateway/v1
request:
  max_body_bytes: {body_limit}
  timeout_ms: 30000
providers:
  - name: primary
    kind: openai
    base_url: {PROVIDER_ADDRESS}
    credential_ref: {credential_ref}
    model_map:
      chat-small: gpt-physical
plugins: []
upstreams:
  - name: api-backend
    scheme: http
    host: 127.0.0.1
    port: 8080
routes:
  - id: health
    path_prefix: /health/
    upstream: api-backend
  - id: llm-chat
    path_prefix: /v1/chat/
    provider: primary
    protocol: {protocol}
"
    )
    .into()
}

/// Reports whether the request path serves the front-end of the fixture route.
///
/// The three facts read here are the ones a converted request depends on: how the bodies relate, which
/// protocol they are translated between, and where the request goes. A publication that stored the
/// plan from another generation would fail at least one of them.
fn front_end_published(harness: &Harness) -> bool {
    let Some(state) = harness.state().current() else {
        return false;
    };
    let Some(route) = state.llm().route("llm-chat") else {
        return false;
    };
    route.converting().is_some()
        && route.protocol() == "openai.chat"
        && route.provider() == "openai"
        && route.path() == "/v1/chat/completions"
        && route.destination().port() == 8080
        && route
            .headers()
            .iter()
            .any(|(name, value)| name == "authorization" && value == &format!("Bearer {KEY}"))
}

#[tokio::test(flavor = "current_thread")]
async fn an_ai_route_is_published_with_its_snapshot_and_survives_a_refused_plan() {
    tokio::time::pause();
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let credential = token::secret_file(directory.path(), "provider.key", KEY);
    let credential_path = credential
        .strip_prefix("file:")
        .map(Path::new)
        .expect("the fixture reference names a file")
        .to_path_buf();

    let source = ChannelSource::new("channel");
    source.push_load(Ok(ConfigVersion::new(
        Revision::comparable(1),
        document(&credential, "openai.chat", 1024),
    )));
    let harness = Harness::start(Arc::clone(&source) as Arc<dyn ConfigSource>);

    let status = harness.advance_until(Harness::ready).await;
    assert_eq!(status.generation, 1);
    assert_eq!(
        harness.state().current().map(|state| state.llm().len()),
        Some(1),
        "the generation that published the snapshot published one front-end with it"
    );
    assert!(
        front_end_published(&harness),
        "the route's protocol, provider, path, destination and credential are the document's"
    );

    // The protocol becomes one this build does not serve. The document model accepts the spelling —
    // the registry is what refuses it — so the plan refuses the publication, and the previously
    // published pair keeps serving.
    source
        .push_comparable(2, &document(&credential, "openai.responses", 2048))
        .await;
    let status = harness
        .advance_until(|harness| harness.status().degraded)
        .await;

    assert_eq!(
        status.generation, 1,
        "a refused LLM plan does not advance the generation"
    );
    assert_eq!(
        harness.body_limit(),
        Some(1024),
        "the document in service is still the one whose plan compiled"
    );
    assert!(
        front_end_published(&harness),
        "a refused publication must not leave the request path without its front-end"
    );

    // Removing the credential refuses the next document for a different reason, and the front-end in
    // service is still the one that was built while the key was readable.
    std::fs::remove_file(&credential_path).expect("the credential fixture is removed");
    source
        .push_comparable(3, &document(&credential, "openai.chat", 4096))
        .await;
    harness
        .advance_until(|harness| harness.status().degraded)
        .await;
    assert_eq!(harness.body_limit(), Some(1024));
    assert!(
        front_end_published(&harness),
        "the credential in service is the one that was materialized, not re-read per request"
    );

    // Restoring the key lets the next document compile, and the snapshot and the plan move together.
    std::fs::write(&credential_path, KEY).expect("the credential fixture is restored");
    source
        .push_comparable(4, &document(&credential, "openai.chat", 8192))
        .await;
    let status = harness
        .advance_until(|harness| harness.generation() == 2)
        .await;

    assert_eq!(status.generation, 2);
    assert!(!status.degraded, "a later success clears the degraded flag");
    assert_eq!(harness.body_limit(), Some(8192));
    assert!(front_end_published(&harness), "the plan moved with it");

    harness.stop().await.expect("shutdown ends the pipeline");
}
