//! The AI role's compilation surface, as the reload pipeline sees it.
//!
//! The pipeline needs exactly one thing from the AI role — "compile this document, or refuse it" —
//! and the traffic role must be able to answer it without compiling the AI crate at all. These three
//! traits are that seam: the pipeline names them, the `llm` feature supplies the real implementation,
//! and the traffic build supplies a refusal.
//!
//! Keeping them here rather than in the `llm` module is what makes the dependency direction hold: a
//! traffic build has no `nico-llm`, so the pipeline cannot name it, and the role a build serves stays
//! a property of the artifact rather than a runtime flag a bootstrap file could set wrong.

use std::sync::Arc;

use nico_core::ErrorCategory;

#[cfg(feature = "llm")]
use crate::llm::LlmRuntime;

/// Compiles one document's provider routes into the front-ends the request path serves.
///
/// The pipeline needs exactly one thing from the AI role — "compile this document, or refuse it" —
/// and nothing else, so that is the whole contract. The `llm` feature supplies the real compiler;
/// the traffic role supplies `NoLlmCompiler`, which refuses every provider route, so a traffic-only
/// build cannot silently serve an AI route it has no codecs for.
///
/// This trait exists so the reload pipeline names no `nico-llm` type: the role a build serves is a
/// property of the artifact, and the pipeline that publishes a generation is the same code either way.
pub trait LlmCompiler: Send + Sync {
    /// Compiles `document`'s provider routes against `chains`.
    ///
    /// # Errors
    /// Returns the refusal that keeps the previously published generation in service. Its
    /// [`LlmCompileError::category`] is what the publication log reports, so the pipeline never has
    /// to know which crate produced it.
    fn compile(
        &self,
        document: &nico_core::Document,
        chains: &nico_http::PublishedChains,
    ) -> Result<Arc<dyn LlmFrontEnds>, LlmCompileError>;
}

/// Why the AI role refused a document.
///
/// A boxed cause rather than a concrete type: the pipeline only renders the message and reads the
/// category, and naming the AI role's error type here would put `nico-llm` back in the pipeline's
/// vocabulary — which is exactly what this trait exists to avoid.
pub struct LlmCompileError {
    category: ErrorCategory,
    source: Box<dyn std::error::Error + Send + Sync>,
}

impl LlmCompileError {
    /// Wraps one refusal with the category the publication log reports.
    ///
    /// Crate-internal on purpose: this is the one value the pipeline can hold without naming the AI
    /// role's error type, so only the compiler that produced a refusal may build one. A caller outside
    /// the crate reads [`LlmCompileError::category`] and the rendered message, which is the whole
    /// contract — the boxed cause is a diagnostic, not something to match on.
    #[must_use]
    pub(crate) fn new(
        category: ErrorCategory,
        source: Box<dyn std::error::Error + Send + Sync>,
    ) -> Self {
        Self { category, source }
    }

    /// Returns how the refusal is classified.
    #[must_use]
    pub const fn category(&self) -> ErrorCategory {
        self.category
    }
}

impl std::fmt::Debug for LlmCompileError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LlmCompileError")
            .field("category", &self.category)
            .field("source", &self.source)
            .finish()
    }
}

impl std::fmt::Display for LlmCompileError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(formatter)
    }
}

impl std::error::Error for LlmCompileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.source)
    }
}

/// The compiled front-ends one generation carries, as the pipeline sees them.
///
/// Deliberately opaque: the pipeline publishes the value and reports how many routes it covers, and
/// never looks inside one. That is what lets the traffic role's compiler produce nothing while the
/// pipeline stays identical.
pub trait LlmFrontEnds: Send + Sync {
    /// Returns how many routes the compiled plan serves.
    fn route_count(&self) -> usize;

    /// Emits the publication log once the generation is in service.
    fn log_published(&self);

    /// Returns the routes the data plane looks up by route id.
    ///
    /// The pipeline publishes this value and never inspects it: it is the byte seam `nico-http`
    /// defines, and naming that trait here is naming `nico-http`, which the traffic role has.
    fn routes(&self) -> Arc<dyn nico_http::llm::LlmRoutes>;
}

/// The AI role's compiler, or the traffic role's refusal when the feature is off.
#[cfg(feature = "llm")]
pub(super) fn builtin_llm() -> Arc<dyn LlmCompiler> {
    Arc::new(LlmRuntime::builtin())
}

/// The traffic role's compiler: it compiles nothing, and refuses a document that asks for AI routes.
#[cfg(not(feature = "llm"))]
pub(super) fn builtin_llm() -> Arc<dyn LlmCompiler> {
    Arc::new(NoLlmCompiler)
}

/// The compiler a traffic-only build uses.
///
/// It refuses a document with a provider route rather than publishing one: an AI route this build
/// has no codecs for would fail every request, and a refused document keeps the last known good
/// generation serving instead.
#[cfg(not(feature = "llm"))]
#[derive(Debug, Clone, Copy)]
pub struct NoLlmCompiler;

#[cfg(not(feature = "llm"))]
impl LlmCompiler for NoLlmCompiler {
    fn compile(
        &self,
        document: &nico_core::Document,
        _chains: &nico_http::PublishedChains,
    ) -> Result<Arc<dyn LlmFrontEnds>, LlmCompileError> {
        let provider_routes = document
            .routes()
            .iter()
            .filter(|route| matches!(route.target(), nico_core::RouteTarget::Provider(_)))
            .count();
        if provider_routes > 0 {
            // The document asks for a role this build does not have: the document's mistake, and the
            // same category the full build's "unknown provider kind" refusal carries.
            return Err(LlmCompileError::new(
                ErrorCategory::Client,
                Box::new(NoLlmRole { provider_routes }),
            ));
        }
        Ok(Arc::new(NoFrontEnds))
    }
}

/// Why a traffic-only build refused a document.
#[cfg(not(feature = "llm"))]
#[derive(Debug, thiserror::Error)]
#[error("this build serves traffic only: {provider_routes} provider route(s) cannot be served")]
struct NoLlmRole {
    provider_routes: usize,
}

/// The empty plan a traffic-only build publishes.
#[cfg(not(feature = "llm"))]
struct NoFrontEnds;

#[cfg(not(feature = "llm"))]
impl LlmFrontEnds for NoFrontEnds {
    fn route_count(&self) -> usize {
        0
    }

    fn log_published(&self) {}

    fn routes(&self) -> Arc<dyn nico_http::llm::LlmRoutes> {
        Arc::new(nico_http::llm::NoLlmRoutes)
    }
}
