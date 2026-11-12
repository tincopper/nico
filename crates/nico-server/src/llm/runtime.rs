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

use super::{CredentialResolver, EnvFileCredentials, LlmBuildError, LlmPlan, SharedServices};

/// The registries, the credential resolver and the shared state one process publishes LLM routes
/// with.
///
/// All three outlive every reload: the registries hold configuration, the resolver holds nothing at
/// all (a key is re-read on every publication, which is the moment an operator expects a rotated one
/// to be picked up), and the shared services hold what the request path **learned** — breaker
/// history that survives a generation and dies with the target that left the document.
pub struct LlmRuntime {
    protocols: ProtocolRegistry,
    providers: ProviderRegistry,
    credentials: Arc<dyn CredentialResolver>,
    shared: Arc<SharedServices>,
}

impl LlmRuntime {
    /// Builds the runtime this build serves: the registry [`nico_llm::builtin_registry`] returns and
    /// the environment/filesystem resolver a deployment reads its keys from.
    #[must_use]
    pub fn builtin() -> Self {
        let (protocols, providers) = nico_llm::builtin_registry();
        log_registries(&protocols, &providers);
        Self::with_parts(protocols, providers, Arc::new(EnvFileCredentials::new()))
    }

    /// Builds a runtime from parts a caller assembled, with breaker state of its own.
    ///
    /// The seam a test uses to compile a document without touching the environment, and the seam a
    /// future assembly uses to add a codec or an adapter without this module changing. The shared
    /// state is this runtime's own; a caller that must share it with another generation uses
    /// [`LlmRuntime::with_shared`].
    #[must_use]
    pub fn with_parts(
        protocols: ProtocolRegistry,
        providers: ProviderRegistry,
        credentials: Arc<dyn CredentialResolver>,
    ) -> Self {
        Self::with_shared(
            protocols,
            providers,
            credentials,
            Arc::new(SharedServices::new()),
        )
    }

    /// Builds a runtime over breaker state the caller owns.
    ///
    /// The seam the inheritance rule needs. "An unchanged target keeps its history, a retuned one
    /// starts over" is a statement about two compilations against **one** table, so it can only be
    /// asserted when the table is not created inside the compile.
    ///
    /// A process passes the single [`SharedServices`] it assembled, which is what makes every
    /// generation a reload publishes observe the same targets. A second table means a second set of
    /// counts, and a breaker that halves every reload.
    #[must_use]
    pub fn with_shared(
        protocols: ProtocolRegistry,
        providers: ProviderRegistry,
        credentials: Arc<dyn CredentialResolver>,
        shared: Arc<SharedServices>,
    ) -> Self {
        Self {
            protocols,
            providers,
            credentials,
            shared,
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
            &Arc::clone(&self.shared),
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

/// Reports which protocols and providers this build assembled, one line per registered side.
///
/// An operator reads what the process can serve before a request ever runs: the registry key, the
/// capability axes that side declares, and — only when the vendor has one — the wire revision. The
/// revision is a field and never a placeholder: a protocol with no versioned spelling reports none,
/// and writing a value on the wire's behalf would be a value the wire never carried.
pub(super) fn log_registries(protocols: &ProtocolRegistry, providers: &ProviderRegistry) {
    for codec in protocols.iter() {
        let protocol = codec.protocol();
        let capabilities = codec.capabilities();
        match codec.version() {
            Some(version) => tracing::info!(
                llm.protocol = %protocol,
                llm.capabilities = %capabilities,
                llm.version = %version,
                "LLM client protocol registered"
            ),
            None => tracing::info!(
                llm.protocol = %protocol,
                llm.capabilities = %capabilities,
                "LLM client protocol registered"
            ),
        }
    }
    for adapter in providers.iter() {
        let kind = adapter.kind();
        let capabilities = adapter.capabilities();
        match adapter.version() {
            Some(version) => tracing::info!(
                llm.provider = %kind,
                llm.capabilities = %capabilities,
                llm.version = %version,
                "LLM provider registered"
            ),
            None => tracing::info!(
                llm.provider = %kind,
                llm.capabilities = %capabilities,
                "LLM provider registered"
            ),
        }
    }
}

impl crate::reload::LlmCompiler for LlmRuntime {
    fn compile(
        &self,
        document: &Document,
        chains: &PublishedChains,
    ) -> Result<Arc<dyn crate::reload::LlmFrontEnds>, crate::reload::LlmCompileError> {
        let plan = LlmRuntime::compile(self, document, chains).map_err(|error| {
            crate::reload::LlmCompileError::new(error.category(), Box::new(error))
        })?;
        Ok(Arc::new(PublishedPlan { plan }))
    }
}

/// One generation's compiled plan, as the pipeline publishes it.
///
/// The wrapper exists because the pipeline needs three things from one value — a route count, the
/// publication log, and the routes view the data plane reads — and a `LlmPlan` is the routes view
/// itself. Holding it behind an `Arc` here is what lets `routes()` hand the data plane the same
/// compiled plan instead of a second copy.
struct PublishedPlan {
    plan: Arc<LlmPlan>,
}

impl crate::reload::LlmFrontEnds for PublishedPlan {
    fn route_count(&self) -> usize {
        nico_http::llm::LlmRoutes::len(&*self.plan)
    }

    fn log_published(&self) {
        self.plan.log_published();
    }

    fn routes(&self) -> Arc<dyn nico_http::llm::LlmRoutes> {
        Arc::clone(&self.plan) as Arc<dyn nico_http::llm::LlmRoutes>
    }
}
