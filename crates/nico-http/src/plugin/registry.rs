//! The loading seam: a document's `kind` string becomes an implementation here, and nowhere else.
//!
//! The document model keeps `kind` opaque on purpose (`nico-core` owns the envelope, not the
//! vocabulary), so the mapping from a name to code is a registry. Two sources feed it and neither
//! changes it:
//!
//! - **Compile time.** `nico-plugins` ships one factory per built-in kind and collects them in
//!   `builtin_registry()`; the process assembly hands that registry to the compiler. That is the whole
//!   of "a built-in plugin is installed".
//! - **Run time** (a later milestone). A WASM host registers one factory that loads modules by path;
//!   the chain, the phases and [`Outcome`](super::Outcome) stay unchanged.
//!
//! Consequences, all deliberate:
//!
//! - **An unknown kind is a publication failure**, never a skip. A skipped authentication plugin is an
//!   open route, so a document naming a kind no factory answers to is refused and the last known good
//!   generation keeps serving.
//! - **A duplicate registration is refused, not overwritten.** Registering `jwt` twice means two
//!   implementations claim one name, so which one runs would depend on assembly order.
//! - **Configuration validation belongs to the factory**, because the factory owns the kind's field
//!   names. [`PluginFactory::build`] deserializes `spec.config()` into the kind's own strongly typed
//!   configuration with `deny_unknown_fields`, so an unknown field is refused with a field path and
//!   the plugin instance name — still at publication, still keeping the last known good document.

use std::{collections::BTreeMap, fmt, sync::Arc};

use nico_core::{ErrorCategory, PluginSpec};

use super::Plugin;

/// The ordering class a kind declares for the request stage.
///
/// A document's order cannot decide this: an unauthenticated request must not spend a token of
/// another caller's quota, so authentication runs first whatever the document wrote. Each kind states
/// its class once, in its factory, so the compiler sorts by this value instead of matching on `kind`
/// strings — and a kind that arrives from another source (a WASM module, say) declares its class the
/// same way.
///
/// The variant order **is** the execution order: `Authenticate`, then `Limit`, then `Other`. Within
/// one class the document's own order applies. The set may grow, so matches outside this crate must
/// name the variants they handle plus a wildcard, and a new variant belongs where it should run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum PluginPhase {
    /// Establishes who the caller is.
    Authenticate,
    /// Charges the caller against a quota.
    Limit,
    /// Neither of the above: runs after them, in document order.
    Other,
}

/// One kind's implementation: how to name it, what its configuration defaults to, and how to load it.
///
/// A factory is shared by every request the kind serves but is only called while a document is
/// compiled, so it may read the filesystem, materialize a credential or compile a template. It must
/// not perform I/O per request: the values [`PluginFactory::build`] computes are what the request
/// path reads.
///
/// State that must outlive a reload (a token-bucket table, a compiled module cache) belongs to the
/// factory, because publication asks it to build an instance and gives it nothing else: the factory
/// is created once when the process is assembled, and every generation is built by the same one.
pub trait PluginFactory: Send + Sync {
    /// Returns the document `kind` this factory answers to, for example `jwt`.
    ///
    /// The spelling is the registry key: changing it changes the configuration documents must write.
    fn kind(&self) -> &'static str;

    /// Returns the ordering class this kind runs in.
    ///
    /// Required rather than defaulted: a kind that silently defaulted to "last" would be ordered
    /// behind an authentication plugin that must precede it.
    fn phase(&self) -> PluginPhase;

    /// Returns this kind's default configuration as JSON text.
    ///
    /// Documentation and [`PluginInfo::default_config`](super::PluginInfo::default_config) both use
    /// it, so the example an operator reads cannot drift from the one the kind accepts.
    fn default_config(&self) -> &'static str;

    /// Loads one plugin instance from its document entry.
    ///
    /// Everything expensive or fallible happens here, once per publication: `spec.config()` is
    /// deserialized into the kind's own type, credential references are materialized, and the refusal
    /// presentation is compiled.
    ///
    /// # Errors
    /// Returns [`PluginBuildError`] when the entry's configuration is not one this kind accepts (an
    /// unknown field, a missing required field, an out-of-range value) or when a credential reference
    /// cannot be read. The error names the plugin instance and the field at fault, and never quotes a
    /// credential value.
    fn build(&self, spec: &PluginSpec) -> Result<Arc<dyn Plugin>, PluginBuildError>;
}

/// Why one plugin instance could not be built from its document entry.
///
/// This is a **publication** error: the document is refused and the previously published generation
/// keeps serving. `reason` is a caller-visible sentence that names the field or reference at fault;
/// it must never contain a credential value, because it is logged and may be shown to an operator.
///
/// A foreign error type cannot be reused here: `nico-http` may not depend on the crate that owns the
/// kind's configuration (that dependency points the other way), and a `String` alone would lose the
/// plugin instance every message must name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("plugin `{plugin}`: {reason}")]
pub struct PluginBuildError {
    plugin: String,
    reason: String,
    category: ErrorCategory,
}

impl PluginBuildError {
    /// Reports that `plugin` could not be built for `reason`.
    ///
    /// The category is [`ErrorCategory::Internal`] until [`Self::with_category`] says otherwise: a
    /// factory that cannot classify its own failure is reporting a defect in itself, and guessing
    /// "the operator's document" would make a plugin bug look like a configuration typo.
    #[must_use]
    pub fn new(plugin: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            plugin: plugin.into(),
            reason: reason.into(),
            category: ErrorCategory::Internal,
        }
    }

    /// Returns a copy classified as `category`.
    ///
    /// The compiler reports it with the category it is given: a document field the kind does not
    /// accept is [`ErrorCategory::Client`], a credential store that cannot be read is
    /// [`ErrorCategory::Upstream`], and an inconsistent build is [`ErrorCategory::Internal`].
    #[must_use]
    pub fn with_category(self, category: ErrorCategory) -> Self {
        Self { category, ..self }
    }

    /// Returns the plugin instance the failure belongs to.
    #[must_use]
    pub fn plugin(&self) -> &str {
        &self.plugin
    }

    /// Returns why the instance could not be built.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// Returns how this build failure is classified.
    #[must_use]
    pub const fn category(&self) -> ErrorCategory {
        self.category
    }
}

/// Why a factory could not be registered.
///
/// The set may grow, so matches outside this crate must name the variants they handle plus a
/// wildcard.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RegistryError {
    /// Two factories claim the same `kind`, so which one loads a document would be arbitrary.
    #[error("plugin kind `{kind}` is registered twice")]
    DuplicateKind {
        /// The kind both factories answer to.
        kind: String,
    },
}

/// The set of kinds this process can load, keyed by the document's `kind` string.
///
/// Ordered because the compile logs one line per kind: a deterministic order makes two runs of the
/// same document comparable. Lookup is by `&str`, so a request path never allocates to ask whether a
/// kind exists — and the request path does not ask at all once a document is compiled.
#[derive(Default)]
pub struct PluginRegistry {
    factories: BTreeMap<&'static str, Arc<dyn PluginFactory>>,
}

impl PluginRegistry {
    /// Creates an empty registry: no document naming a plugin can be published yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one kind's factory.
    ///
    /// # Errors
    /// Returns [`RegistryError::DuplicateKind`] when a factory for the same [`kind`] is already
    /// registered. Replacing it instead would make the implementation a document gets depend on
    /// assembly order, which is exactly the kind of ambiguity a policy registry must not have.
    ///
    /// [`kind`]: PluginFactory::kind
    pub fn register(&mut self, factory: Arc<dyn PluginFactory>) -> Result<(), RegistryError> {
        let kind = factory.kind();
        if self.factories.contains_key(kind) {
            return Err(RegistryError::DuplicateKind {
                kind: kind.to_owned(),
            });
        }
        self.factories.insert(kind, factory);
        Ok(())
    }

    /// Returns the factory that answers to `kind`, or `None` when no implementation is registered.
    ///
    /// `None` is what makes a document name an unknown kind: the compiler refuses the document rather
    /// than publishing a chain with a plugin silently missing from it.
    #[must_use]
    pub fn factory(&self, kind: &str) -> Option<&Arc<dyn PluginFactory>> {
        self.factories.get(kind)
    }

    /// Returns how many kinds are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.factories.len()
    }

    /// Returns whether no kind is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.factories.is_empty()
    }
}

impl fmt::Debug for PluginRegistry {
    /// Prints the registered kinds, not the factories: a factory's fields include configuration and
    /// credentials, and a registry is logged once per publication.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_set()
            .entries(self.factories.keys().copied())
            .finish()
    }
}
