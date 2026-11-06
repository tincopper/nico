//! Why a document's provider routes could not be compiled into front-ends.
//!
//! Every variant is a **publication** failure: the document is refused, the last known good generation
//! keeps serving, and the operator reads one sentence naming the route. Nothing here can be caused by a
//! request — a request-time refusal is the front-end's, and it keeps the client protocol's own error
//! document.
//!
//! The classification mirrors the plugin compiler's: a document this build cannot serve is the
//! document's mistake ([`ErrorCategory::Client`]) and a credential that cannot be read is a dependency
//! failure ([`ErrorCategory::Upstream`]). No variant carries a credential value, a header value or a
//! request body: this type's `Display` reaches the publication log.

use nico_core::{ErrorCategory, RouteMode};
use nico_llm::{ExchangeBuildError, RouteError};

use super::CredentialError;

/// Why one document's LLM plan could not be compiled.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LlmBuildError {
    /// A provider route names no client protocol.
    ///
    /// The document model already refuses this, so reaching it means the model changed under this
    /// crate: the plan refuses rather than guessing which codec to run.
    #[error("route `{route}` is a provider route without a client protocol")]
    MissingProtocol {
        /// The route that named none.
        route: String,
    },
    /// The route names a client protocol no registered codec serves.
    #[error(
        "route `{route}` names the client protocol `{protocol}`, which this build does not serve"
    )]
    UnknownProtocol {
        /// The route that named it.
        route: String,
        /// The protocol, as the document wrote it.
        protocol: String,
    },
    /// The route names a provider the document does not declare.
    ///
    /// Reference validation already refuses this; reaching it means the model changed under this
    /// crate, and a route that addressed nothing would be worse than a refused document.
    #[error("route `{route}` names the provider `{provider}`, which the document does not declare")]
    MissingProvider {
        /// The route that named it.
        route: String,
        /// The provider name, as the document wrote it.
        provider: String,
    },
    /// The route names a provider kind no registered adapter serves.
    #[error(
        "route `{route}` names provider `{provider}`, whose kind `{kind}` this build does not serve"
    )]
    UnknownProviderKind {
        /// The route that named it.
        route: String,
        /// The provider entry that declares the kind.
        provider: String,
        /// The kind, as the document wrote it.
        kind: String,
    },
    /// The route names plugins, but the plan compiled from the same document has no chain for it.
    ///
    /// The plugin compiler refuses such a document, so reaching this means the two compilers were
    /// handed different documents. Refusing the publication is the only safe answer: serving the route
    /// would run it with no policy at all, which for an authentication plugin is an open door.
    #[error("route `{route}` names plugins, but no compiled chain covers it")]
    MissingPluginChain {
        /// The route whose chain is missing.
        route: String,
    },
    /// The route's mode is one this build does not implement.
    ///
    /// A mode is how bytes move, so guessing the default for an unknown one could convert a body the
    /// operator asked to forward. The document is refused instead.
    #[error("route `{route}` uses the mode `{mode:?}`, which this build does not serve")]
    UnsupportedMode {
        /// The route that asked for it.
        route: String,
        /// The mode the document wrote.
        mode: RouteMode,
    },
    /// The compiled route facts could not be built from the document's provider entry.
    #[error("route `{route}` has unusable route facts: {source}")]
    Route {
        /// The route whose facts are unusable.
        route: String,
        /// The provider entry's own refusal (an empty base URL, an unparseable model map).
        #[source]
        source: RouteError,
    },
    /// The provider's credential could not be materialized.
    #[error("route `{route}` cannot read the credential of provider `{provider}`: {source}")]
    Credential {
        /// The route that needs it.
        route: String,
        /// The provider entry that references it.
        provider: String,
        /// Why the reference could not be read.
        #[source]
        source: CredentialError,
    },
    /// The codec, the adapter and the route could not be assembled into a front-end.
    ///
    /// The converting and the passthrough constructor report the same vocabulary, which is why one
    /// variant carries both: which of the two runs is the route's mode, not a decision a reader of the
    /// log needs to make.
    #[error("route `{route}` cannot be published: {source}")]
    Exchange {
        /// The route that cannot be served.
        route: String,
        /// The constructor's reason, including an endpoint it could not compile.
        #[source]
        source: ExchangeBuildError,
    },
    /// The chain every request runs declares a stage an LLM route cannot offer.
    ///
    /// The check runs only when the document has at least one provider route: a global body stage is
    /// this data plane's business everywhere it does not meet an exchange.
    #[error(
        "global plugin `{plugin}` runs the `{phase}` stage, which an LLM route cannot offer a plugin"
    )]
    GlobalPluginPhase {
        /// The instance that declares the stage.
        plugin: String,
        /// The stage, in the spelling the phase flags use.
        phase: &'static str,
    },
    /// The route's chain declares a stage an LLM route cannot offer.
    #[error(
        "route `{route}` runs plugin `{plugin}` in the `{phase}` stage, which an LLM route cannot offer"
    )]
    RoutePluginPhase {
        /// The route whose chain declares it.
        route: String,
        /// The instance that declares the stage.
        plugin: String,
        /// The stage, in the spelling the phase flags use.
        phase: &'static str,
    },
    /// A plugin asked for the whole request body on a route whose body belongs to its front-end.
    ///
    /// The two are different buffering contracts: a plugin sees a body a policy can rewrite, the
    /// front-end sees the document it decodes (or the bytes it forwards). Running both would hold two
    /// copies with no rule about which one the provider receives, and on a `passthrough` route the
    /// policy's stage would never run at all — an inert policy that believes it inspected the body is
    /// worse than a refused document.
    #[error(
        "route `{route}` buffers the request body for a plugin, which an LLM route cannot offer"
    )]
    BufferedRoute {
        /// The route whose body a plugin asked to buffer.
        route: String,
    },
}

impl LlmBuildError {
    /// How a refused publication is classified.
    ///
    /// A credential read keeps its own category: a missing dependency is not a document the operator
    /// spelled wrong. Everything else is the document's problem, which is the same split the plugin
    /// compiler makes.
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        match self {
            Self::Credential { source, .. } => source.category(),
            // Not the document's problem: the two compilers disagreed about the same bytes.
            Self::MissingPluginChain { .. } => ErrorCategory::Internal,
            Self::MissingProtocol { .. }
            | Self::UnknownProtocol { .. }
            | Self::MissingProvider { .. }
            | Self::UnknownProviderKind { .. }
            | Self::UnsupportedMode { .. }
            | Self::Route { .. }
            | Self::Exchange { .. }
            | Self::GlobalPluginPhase { .. }
            | Self::RoutePluginPhase { .. }
            | Self::BufferedRoute { .. } => ErrorCategory::Client,
        }
    }
}

// Compiled-in proof that a refused publication can cross `.await` and `tokio::spawn` boundaries.
const _: fn() = || {
    const fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<LlmBuildError>();
};
