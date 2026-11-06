//! What a process compiles an LLM plan with: the registries and the credential resolver.
//!
//! The three values travel together because a publication needs all three to compile one route, and
//! because all three outlive every reload: a codec and an adapter hold configuration and no
//! per-request state, and the resolver holds nothing at all — a key is re-read on every publication,
//! which is the moment an operator expects a rotated one to be picked up.
//!
//! The process creates one runtime while it is assembled and hands it to the pipeline; a test creates
//! one from its own parts. Neither path builds a second compilation: [`LlmRuntime::compile`] is the
//! only way an [`LlmPlan`] reaches the request path.

use std::{fmt, sync::Arc};

use nico_core::Document;
use nico_http::PublishedChains;
use nico_llm::{ProtocolRegistry, ProviderRegistry};

use super::{CredentialResolver, EnvFileCredentials, LlmBuildError, LlmPlan};

/// The registries and the credential resolver one process publishes LLM routes with.
pub struct LlmRuntime {
    protocols: ProtocolRegistry,
    providers: ProviderRegistry,
    credentials: Arc<dyn CredentialResolver>,
}

impl LlmRuntime {
    /// Builds the runtime this build serves: the registry [`nico_llm::builtin_registry`] returns and
    /// the environment/filesystem resolver a deployment reads its keys from.
    #[must_use]
    pub fn builtin() -> Self {
        let (protocols, providers) = nico_llm::builtin_registry();
        tracing::info!(
            llm.protocols = ?protocols,
            llm.providers = ?providers,
            "LLM registries assembled"
        );
        Self::with_parts(protocols, providers, Arc::new(EnvFileCredentials::new()))
    }

    /// Builds a runtime from parts a caller assembled.
    ///
    /// The seam a test uses to compile a document without touching the environment, and the seam a
    /// future assembly uses to add a codec or an adapter without this module changing.
    #[must_use]
    pub fn with_parts(
        protocols: ProtocolRegistry,
        providers: ProviderRegistry,
        credentials: Arc<dyn CredentialResolver>,
    ) -> Self {
        Self {
            protocols,
            providers,
            credentials,
        }
    }

    /// Compiles one document's provider routes against this runtime's registries.
    ///
    /// `chains` is the plugin plan compiled from the same document, because a route's body ownership
    /// is one decision: the plan refuses a route that is buffered twice and one whose chain declares a
    /// stage an exchange cannot offer.
    ///
    /// # Errors
    /// Returns [`LlmBuildError`] when a provider route cannot be served. The caller refuses the
    /// document and keeps the previously published generation.
    pub fn compile(
        &self,
        document: &Document,
        chains: &PublishedChains,
    ) -> Result<Arc<LlmPlan>, LlmBuildError> {
        let plan = LlmPlan::compile(
            document,
            &self.protocols,
            &self.providers,
            chains,
            self.credentials.as_ref(),
        )?;
        Ok(Arc::new(plan))
    }
}

impl fmt::Debug for LlmRuntime {
    /// Prints the registered kinds and nothing else: the resolver holds a deployment's key source,
    /// and the values it reads have no spelling here.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LlmRuntime")
            .field("protocols", &self.protocols)
            .field("providers", &self.providers)
            .finish_non_exhaustive()
    }
}

// Compiled-in proof that the one runtime a process assembles can be shared across every thread that
// compiles or serves a generation — which is why the credential resolver is required to be
// `Send + Sync` rather than merely object-safe.
const _: fn() = || {
    const fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<LlmRuntime>();
};
