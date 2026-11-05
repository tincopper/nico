//! The `rate_limit` kind's factory: one document entry, and the table every generation shares.
//!
//! A rate limit is the one built-in kind with state that outlives a publication. The table cannot live
//! in the plugin instance, because a reload rebuilds every instance; and it cannot live in the compiler,
//! because publication asks a factory to build an instance and hands it nothing else. So it lives here,
//! next to the kind that owns it: the process assembles one factory, and every generation is built by
//! it.
//!
//! A table is recorded as a [`Weak`] reference, and the generation in service holds the only strong
//! ones. Two consequences, both wanted:
//!
//! - **An unchanged quota keeps its buckets.** The next publication upgrades the record and shares the
//!   table the old generation is still charging.
//! - **A table nobody can reach is freed.** Renaming an instance, or removing it from the document,
//!   leaves one small record that the next build prunes — not a table kept for the life of the process.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, Weak},
};

use nico_core::PluginSpec;
use nico_http::{Plugin, PluginBuildError, PluginFactory, PluginPhase};

use super::{DEFAULT_CONFIG, LimitParameters, Limiter, RateLimitConfig, RateLimitPlugin};
use crate::{build_failure, spec::RateLimitSpec};

/// Loads the `rate_limit` kind and keeps its bucket tables across publications.
pub(crate) struct RateLimitFactory {
    tables: Mutex<HashMap<String, Weak<Limiter>>>,
}

impl RateLimitFactory {
    /// Creates a factory with no tables.
    pub(crate) fn new() -> Self {
        Self {
            tables: Mutex::new(HashMap::new()),
        }
    }

    /// Returns the table `name` is charged against, reusing the one a live generation already holds.
    ///
    /// The parameters are the identity of a quota: the same numbers mean the same buckets keep
    /// serving, and a change means the operator changed the quota, which is a reset by definition.
    fn table(&self, name: &str, parameters: LimitParameters) -> Arc<Limiter> {
        let mut tables = self.lock();
        // Drop the records of tables no generation holds any more. Safe to run before a build: a table
        // this publication will reuse is, by definition, still held by the generation in service.
        tables.retain(|_, table| table.strong_count() > 0);
        if let Some(existing) = tables.get(name).and_then(Weak::upgrade) {
            if existing.parameters() == parameters {
                return existing;
            }
        }
        let limiter = Arc::new(Limiter::new(parameters));
        tables.insert(name.to_owned(), Arc::downgrade(&limiter));
        limiter
    }

    /// Returns the table currently in service for `name`, for tests and diagnostics.
    #[cfg(test)]
    pub(crate) fn current(&self, name: &str) -> Option<Arc<Limiter>> {
        self.lock().get(name).and_then(Weak::upgrade)
    }

    /// Takes the table map's lock, recovering from a poisoned one.
    ///
    /// Nothing that runs under the lock can panic, so a poisoned guard still describes a consistent
    /// map, and recovering keeps a publication alive instead of turning a panic elsewhere into a
    /// permanently unusable factory.
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Weak<Limiter>>> {
        match self.tables.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl Default for RateLimitFactory {
    /// Returns [`RateLimitFactory::new`].
    fn default() -> Self {
        Self::new()
    }
}

impl PluginFactory for RateLimitFactory {
    fn kind(&self) -> &'static str {
        "rate_limit"
    }

    fn phase(&self) -> PluginPhase {
        // A token is spent, not earned: whatever establishes the caller must have run first.
        PluginPhase::Limit
    }

    fn default_config(&self) -> &'static str {
        DEFAULT_CONFIG
    }

    fn build(&self, spec: &PluginSpec) -> Result<Arc<dyn Plugin>, PluginBuildError> {
        let config =
            RateLimitConfig::from_spec(spec).map_err(|error| build_failure(spec.name(), error))?;
        let compiled = RateLimitSpec::compile(spec, &config)
            .map_err(|error| build_failure(spec.name(), error))?;
        let limiter = self.table(spec.name(), compiled.parameters());
        Ok(Arc::new(RateLimitPlugin::new(&compiled, limiter)))
    }
}
