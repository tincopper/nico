//! Built-in HTTP policies: HS256 JWT authentication, static API keys and token-bucket rate limiting.
//!
//! A plugin is a named entry of the configuration document. This crate owns what such an entry
//! *means*: which phase it runs in, what a refusal looks like, and where its credentials come from.
//! [`nico_core`] owns the shape of the entry and [`nico_http`] owns the plugin contract and the single
//! place a failure becomes a response, so nothing here writes a response, reads a request body or
//! decides a status code for anything but its own refusal.
//!
//! # Modules
//!
//! - `spec` — a document entry compiled into the parameters a plugin runs with, so the request path
//!   never re-parses configuration.
//! - `compile` — the document as a whole turned into the per-route chains a generation runs, by
//!   asking the [`PluginRegistry`] for the factory that answers each entry's `kind`.
//! - `rejection` — the compiled refusal of one plugin instance: its status, content type and body
//!   template, plus the built-in presentation each kind falls back to.
//! - `jwt` — bearer token parsing, HS256 verification, claim validation, and the identity it
//!   establishes. Its `factory` module is the loading seam for the `jwt` kind.
//! - `api_key` — a static key set compared in constant time, read from one configured header.
//! - `rate_limit` — a token bucket per caller, kept in a bounded table that survives a reload. The
//!   table belongs to the kind's factory, because a publication asks that factory to build an
//!   instance and keeps no plugin state of its own.
//!
//! # Invariants
//!
//! - **A refusal is an error, never a response.** A plugin returns [`PluginError::Refused`] and the
//!   data plane writes it once, with a closed header set.
//! - **Configuration fails at publication, not on the request path.** Templates, statuses, ranges and
//!   credentials are compiled when a document is published; a request only reads the result.
//! - **Credentials never leave the process.** An error names the plugin and the credential
//!   *reference*; a token, a key and an IP address are never logged and never rendered.
//! - **Identity cannot be forged.** Only an authentication plugin writes it, and only after it
//!   verified the credential.
//! - **`kind` is a string, and this crate answers it.** `nico-core` holds no kind vocabulary, so
//!   [`builtin_registry`] is the only place a document's `kind` becomes an implementation. An
//!   unknown kind is a publication failure, never a skip.
//!
//! [`PluginError::Refused`]: nico_http::PluginError::Refused

#![deny(missing_docs)]

mod api_key;
mod compile;
mod jwt;
mod rate_limit;
mod rejection;
mod spec;
#[cfg(feature = "wasm")]
pub mod wasm;

use std::{borrow::Cow, error::Error, sync::Arc};

pub use compile::PluginRuntime;
use nico_core::{ErrorCategory, PluginSpec};
use nico_http::{PluginBuildError, PluginFactory, PluginRegistry};
use serde::de::DeserializeOwned;

/// Returns the registry of every kind this build implements.
///
/// This is the compile-time half of "two ways to load one registry": the process assembly hands the
/// result to [`PluginRuntime::with_registry`], and a WASM host will add its own factory to the same
/// registry without this function changing. A kind's factory owns everything expensive and fallible —
/// the kind's configuration type, credential materialization, the refusal template — so publication
/// only ever asks it to build an instance.
#[must_use]
pub fn builtin_registry() -> PluginRegistry {
    let mut registry = PluginRegistry::new();
    let factories: [Arc<dyn PluginFactory>; 3] = [
        Arc::new(jwt::JwtFactory),
        Arc::new(api_key::ApiKeyFactory),
        Arc::new(rate_limit::RateLimitFactory::new()),
    ];
    for factory in factories {
        if let Err(error) = registry.register(factory) {
            // Unreachable by construction: the three kinds above are distinct. Reported rather than
            // asserted because a panic while the process is assembled is worse than a duplicate
            // registration, and the test that asserts all three kinds resolve catches it.
            tracing::error!(%error, "a built-in plugin kind was registered twice");
        }
    }
    registry
}

/// The registry the process serves with: the built-in kinds, plus the WASM host when the build
/// carries it.
///
/// A build without the `wasm` feature answers `kind: wasm` with the registry's unknown-kind
/// refusal — the same answer a misspelt kind gets — which is the right shape for a deployment that
/// never ships a WASM artifact: the document is refused, not silently skipped.
#[must_use]
pub fn full_registry() -> PluginRegistry {
    #[cfg(not(feature = "wasm"))]
    return builtin_registry();

    #[cfg(feature = "wasm")]
    {
        let mut registry = builtin_registry();
        if let Err(error) = registry.register(Arc::new(wasm::WasmFactory)) {
            tracing::error!(%error, "the wasm plugin kind was registered twice");
        }
        registry
    }
}

/// Converts one kind's publication failure into the loading seam's error.
///
/// The seam cannot name this crate's error type (`nico-http` may not depend on the crate that owns a
/// kind), so the message and the category are carried across as data. The message is safe to log —
/// every variant names a plugin and a field or reference, never a credential value — and the
/// category is kept so a credential store that cannot be read is not reported as a document typo.
pub(crate) fn build_failure(plugin: &str, error: PluginError) -> PluginBuildError {
    PluginBuildError::new(plugin, error.to_string()).with_category(error.category())
}

/// Why a plugin could not be compiled from its document entry.
///
/// Every variant is a *publication* failure: the document is refused, the previously published
/// generation keeps serving, and the message names the plugin and the field or reference at fault.
/// None of them can be produced by a request, and none of them may quote a credential value — the
/// reference (`env:NAME`) is not the secret, and only the reference is ever reported.
///
/// The set may grow, so matches outside this crate must handle the variants they know plus a
/// wildcard.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PluginError {
    /// A rejection presentation this crate was handed is not one the data plane can write.
    ///
    /// The document model already refuses a non-4xx status and an unprintable content type, so this
    /// is the second half of one rule: the plugin layer re-checks the presentation it compiles
    /// instead of trusting the shape it was given.
    #[error("plugin `{plugin}` has an invalid rejection presentation: {source}")]
    Rejection {
        /// The plugin instance the presentation belongs to.
        plugin: String,
        /// Which rule the presentation broke.
        #[source]
        source: nico_http::RejectionError,
    },

    /// A rejection body names a `${...}` variable outside the whitelist.
    #[error(
        "plugin `{plugin}` has an invalid rejection template: `${{{variable}}}` is not one of `code`, `message` or `trace_id`"
    )]
    UnknownVariable {
        /// The plugin instance the template belongs to.
        plugin: String,
        /// The rejected variable name, sanitized so a control character cannot reach a log record.
        variable: String,
    },

    /// A rejection body opens `${` and never closes it.
    #[error("plugin `{plugin}` has an invalid rejection template: an unclosed `${{` placeholder")]
    UnterminatedTemplate {
        /// The plugin instance the template belongs to.
        plugin: String,
    },

    /// A non-JSON content type was configured for a body this crate would render as JSON.
    ///
    /// The built-in presentation is the JSON envelope, so a content type that is not JSON has no
    /// body to describe unless the entry provides one. Writing one label over the other shape would
    /// make every caller's parser see something the header denies.
    #[error(
        "plugin `{plugin}` sets a non-JSON content type without a body; a custom content type needs a custom body"
    )]
    ContentTypeWithoutBody {
        /// The plugin instance that configured it.
        plugin: String,
    },

    /// A parameter is outside the range this build accepts.
    ///
    /// The document model validates every one of these too; the plugin layer checks again because it
    /// converts them into types the request path can use without checking (`NonZeroU32`, `Duration`),
    /// and a value that cannot be converted must not become a panic later.
    #[error("plugin `{plugin}` has an invalid `{field}`: {reason}")]
    InvalidParameter {
        /// The plugin instance the parameter belongs to.
        plugin: String,
        /// The configuration field, for example `burst`.
        field: &'static str,
        /// Why the value was refused. Never quotes a credential.
        reason: Cow<'static, str>,
    },

    /// A credential reference could not be materialized while the document was compiled.
    ///
    /// The reference is the whole story: it is what the operator wrote, and the value it names is
    /// exactly what must not appear here. `cause` is a sentence this crate wrote for the kind of
    /// failure — "the credential is empty", "the environment variable is not set" — so the message
    /// says what to fix without ever rendering the underlying error, whose text could quote a value.
    #[error("plugin `{plugin}` cannot read its credential reference `{reference}`: {cause}")]
    SecretRef {
        /// The plugin instance that names the reference.
        plugin: String,
        /// The reference as written, for example `env:JWT_SECRET`.
        reference: String,
        /// A fixed sentence describing what is wrong with the reference; safe to log.
        cause: Cow<'static, str>,
        /// Why reading it failed, kept for a caller that wants the cause.
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },

    /// A registered kind could not build one entry from its document configuration.
    ///
    /// The kind's factory owns the field names, so it is the only thing that can say which field is
    /// wrong, which credential reference cannot be read, or which presentation it cannot compile.
    /// The reason it reports is carried across the loading seam as text: it names the field or the
    /// reference, never a credential value, and the category travels with it so a failure of a
    /// dependency is not reported as the operator's mistake.
    #[error("plugin `{plugin}` could not be built from its config: {reason}")]
    BuildFailed {
        /// The plugin instance whose kind refused to build it.
        plugin: String,
        /// Why, as the kind's factory reported it.
        reason: String,
        /// How the kind classified the failure.
        category: ErrorCategory,
    },

    /// An entry's `config` value is not one this kind accepts.
    ///
    /// The document model treats `config` as opaque, so the kind's own structure is read here with
    /// `deny_unknown_fields`: an unknown field, a wrong type, or a `config` that is not a mapping is
    /// refused while the document is compiled. The cause carries the field path serde produced.
    #[error("plugin `{plugin}` has an invalid `config`: {source}")]
    InvalidConfig {
        /// The plugin instance whose configuration could not be read.
        plugin: String,
        /// Why the value could not be read, naming the offending field.
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },

    /// An entry names a `kind` that no registered implementation answers to.
    ///
    /// The document model does not know kinds — `kind` is a string there — so this is where "does
    /// this build implement it?" is decided. The entry is refused rather than skipped: a skipped
    /// plugin is a route that runs *fewer* policies than it names, which for an authentication plugin
    /// is an open door. The message quotes the kind so the operator can see the typo.
    #[error("plugin `{plugin}` has kind `{kind}`, which this build does not implement")]
    UnknownKind {
        /// The plugin instance whose kind nothing answers to.
        plugin: String,
        /// The kind exactly as the document spelled it.
        kind: String,
    },

    /// A route names a plugin the compiled plan does not contain.
    ///
    /// The document model rejects a dangling plugin reference, so reaching this means the compiler
    /// produced an incomplete plan. It is refused instead of published: [`RouteChains::chain_for`]
    /// returning `None` on the request path means "this route has no policy", so a plan that skipped
    /// a route would silently disable the policies that route declared.
    ///
    /// [`RouteChains::chain_for`]: nico_http::RouteChains::chain_for
    #[error("route `{route}` names plugin `{plugin}`, which the compiled plan does not cover")]
    MissingChain {
        /// The route whose policy list has no chain.
        route: String,
        /// The name the route lists that no compiled plugin answers to.
        plugin: String,
    },

    /// The caller materialized a different number of credentials than the entry declares.
    ///
    /// This is not a document an operator can write: it means the compiler that builds filters
    /// produced an inconsistent pair. It is reported rather than padded or truncated, because either
    /// would silently drop a key from an accepted set.
    #[error(
        "plugin `{plugin}` declares {declared} credentials but {materialized} were materialized"
    )]
    CredentialCountMismatch {
        /// The plugin instance whose credential list does not match.
        plugin: String,
        /// How many credentials the entry declares.
        declared: usize,
        /// How many were handed to the filter.
        materialized: usize,
    },
}

impl PluginError {
    /// Returns how this compilation failure is classified.
    ///
    /// A document the operator wrote wrong is [`ErrorCategory::Client`]; a secret store that could
    /// not be read is a dependency this process relies on, so [`ErrorCategory::Upstream`]; an
    /// inconsistent credential list is a defect in this crate, so [`ErrorCategory::Internal`]. The
    /// category follows the cause, never the fact that publication failed.
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        match self {
            Self::Rejection { .. }
            | Self::UnknownVariable { .. }
            | Self::UnterminatedTemplate { .. }
            | Self::ContentTypeWithoutBody { .. }
            | Self::InvalidParameter { .. }
            | Self::InvalidConfig { .. }
            | Self::UnknownKind { .. } => ErrorCategory::Client,
            Self::SecretRef { .. } => ErrorCategory::Upstream,
            Self::BuildFailed { category, .. } => *category,
            Self::CredentialCountMismatch { .. } | Self::MissingChain { .. } => {
                ErrorCategory::Internal
            }
        }
    }
}

/// Reads one entry's `config` value as this kind's own configuration.
///
/// The document model treats `config` as opaque, so this is the one place a value written by an
/// operator becomes a typed structure. An entry that writes no `config` carries `Value::Null`; a kind
/// reads a mapping, so "wrote no fields at all" is spelled as an empty mapping and every
/// `#[serde(default)]` in that kind's type applies.
///
/// # Errors
/// Returns [`PluginError::InvalidConfig`] when `value` has a field the kind does not have, a field of
/// the wrong type, or a shape that is not a mapping at all. The message names the plugin and the
/// field path.
pub(crate) fn config_value<T: DeserializeOwned>(spec: &PluginSpec) -> Result<T, PluginError> {
    let value = match spec.config() {
        serde_norway::Value::Null => serde_norway::Value::Mapping(serde_norway::Mapping::new()),
        written => written.clone(),
    };
    serde_norway::from_value(value).map_err(|source| PluginError::InvalidConfig {
        plugin: spec.name().to_owned(),
        source: Box::new(source),
    })
}

/// Builds the publication error for one rejected configuration field.
///
/// Every kind reports its own rules through this constructor, so the message names the plugin and the
/// field below `config` in one shape. `reason` is taken by `Into<Cow>`: a rule that quotes a limit
/// formats it from the constant the check used, so the message and the check cannot disagree.
pub(crate) fn invalid_parameter(
    plugin: &str,
    field: &'static str,
    reason: impl Into<Cow<'static, str>>,
) -> PluginError {
    PluginError::InvalidParameter {
        plugin: plugin.to_owned(),
        field,
        reason: reason.into(),
    }
}

// Compiled-in proof that a publication failure can cross `.await` and `tokio::spawn` boundaries.
const _: fn() = || {
    const fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<PluginError>();
};

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod tests;
