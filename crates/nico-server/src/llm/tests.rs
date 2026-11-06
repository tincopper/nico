//! What a document compiles into, and what refuses it.
//!
//! Every test here compiles a real document with the real registries: the plan's job is to pair the
//! values a deployment actually assembles, so a mock codec or adapter would assert the mocks. The two
//! exceptions are deliberate — a resolver that answers from a map (reading the environment is `unsafe`
//! in the 2024 edition and shared with every test in the binary) and an adapter that reports a native
//! protocol no codec speaks, which is the only way to reach the passthrough mismatch with one real
//! protocol in the build.
//!
//! The fixtures live here so the two scenario files share one definition of "a document with a
//! provider route": a change to the schema updates one fixture, not one per file.

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use nico_core::{Document, ParseLimits};
use nico_http::{
    Plugin, PluginChain, PluginEntry, PluginInfo, PluginPhases, PublishedChains, ResponseRewrites,
    RouteChains,
};
use nico_llm::{Credential, ProtocolRegistry, ProviderRegistry};

use super::{
    CredentialError, CredentialResolver, EnvFileCredentials, LlmBuildError, LlmPlan, LlmRuntime,
};

/// The provider section most fixtures use: one endpoint, one credential reference.
const PROVIDER: &str = "\
providers:
  - name: primary
    kind: openai
    base_url: http://127.0.0.1:8080
    credential_ref: env:PRIMARY_KEY
";

/// A converting route to the fixture provider.
const CONVERT: &str = "\
routes:
  - id: llm
    path_prefix: /v1/
    provider: primary
    protocol: openai.chat
";

/// A passthrough route to the fixture provider.
const PASSTHROUGH: &str = "\
routes:
  - id: llm
    path_prefix: /v1/
    provider: primary
    protocol: openai.chat
    mode: passthrough
";

/// Parses one fixture document around `providers`, `plugins`, `upstreams` and `routes` sections.
fn document(sections: &str) -> Document {
    let text = format!(
        "\
apiVersion: gateway/v1
request:
  max_body_bytes: 1048576
  timeout_ms: 30000
{sections}"
    );
    Document::parse(text.as_bytes(), ParseLimits::new(64 * 1024))
        .expect("the fixture document parses")
}

/// Parses the fixture provider plus `sections`, with the two sections a document requires empty.
fn with_provider(sections: &str) -> Document {
    document(&format!("{PROVIDER}plugins: []\nupstreams: []\n{sections}"))
}

/// The registries a process assembles.
fn registries() -> (ProtocolRegistry, ProviderRegistry) {
    nico_llm::builtin_registry()
}

/// A resolver that answers from a map and counts how often it was asked.
///
/// The count is the subject of one test: a plan must materialize one credential per provider, not one
/// per route, because a `file:` reference would otherwise be re-read per route.
#[derive(Default)]
struct MapCredentials {
    values: BTreeMap<String, String>,
    reads: AtomicUsize,
}

impl MapCredentials {
    fn new(pairs: &[(&str, &str)]) -> Arc<Self> {
        Arc::new(Self {
            values: pairs
                .iter()
                .map(|(reference, value)| ((*reference).to_owned(), (*value).to_owned()))
                .collect(),
            reads: AtomicUsize::new(0),
        })
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

impl CredentialResolver for MapCredentials {
    fn resolve(&self, reference: &str) -> Result<Credential, CredentialError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        match self.values.get(reference) {
            Some(value) => Ok(Credential::new(value.clone())),
            None => Err(CredentialError::unusable(
                reference,
                "the fixture resolver has no value for this reference",
            )),
        }
    }
}

/// A plugin whose whole value is the stages it declares.
///
/// The data plane never runs it — the plan only reads the declaration — so the trait's methods keep
/// their default bodies.
struct Declared {
    name: &'static str,
    phases: PluginPhases,
}

impl Plugin for Declared {
    fn name(&self) -> &str {
        self.name
    }

    fn info(&self) -> PluginInfo {
        PluginInfo::new("0.0.0", "a stage declaration for a plan test", "{}")
            .with_phases(self.phases)
    }
}

/// Builds a chain of one declared plugin.
fn chain(name: &'static str, phases: PluginPhases) -> PluginChain {
    let entry = PluginEntry::new(Arc::new(Declared { name, phases }), Arc::from("{}"));
    PluginChain::new(vec![entry].into())
}

/// Builds a published plan from the chains a test wants to confront the LLM plan with.
fn chains(
    global: PluginChain,
    routes: &[(&str, PluginChain)],
    buffered: &[&str],
) -> PublishedChains {
    PublishedChains::new(
        global,
        routes
            .iter()
            .map(|(id, chain)| ((*id).to_owned(), chain.clone()))
            .collect::<RouteChains>(),
        buffered.iter().map(|id| (*id).to_owned()).collect(),
        ResponseRewrites::empty(),
    )
}

/// Compiles `document` with the fixture resolver.
fn compile(document: &Document) -> Result<LlmPlan, LlmBuildError> {
    compile_with(document, &PublishedChains::empty())
}

/// Compiles `document` against `chains` with the fixture resolver.
fn compile_with(document: &Document, chains: &PublishedChains) -> Result<LlmPlan, LlmBuildError> {
    let (protocols, providers) = registries();
    let credentials = MapCredentials::new(&[("env:PRIMARY_KEY", "sk-primary")]);
    LlmPlan::compile(
        document,
        &protocols,
        &providers,
        chains,
        credentials.as_ref(),
    )
}

mod plan;
mod runtime;
