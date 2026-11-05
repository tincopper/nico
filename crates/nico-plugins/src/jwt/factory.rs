//! The `jwt` kind's factory: the one place a `jwt` entry becomes a running plugin.
//!
//! The factory owns everything expensive and fallible about the kind, so publication only asks it to
//! build an instance and the request path reads the result:
//!
//! 1. [`JwtConfig`] reads `config` with `deny_unknown_fields` — the field names are this kind's, not
//!    the document model's;
//! 2. [`JwtSpec`] turns those values into the types a request can use without checking, and compiles
//!    both refusals;
//! 3. the credential references are materialized — the only I/O a publication performs for a plugin;
//! 4. the plugin is built over the materialized key set.
//!
//! A failure at any step keeps the last known good document in service: it never becomes a request
//! failure, and it never quotes a credential value.

use std::sync::Arc;

use nico_core::PluginSpec;
use nico_http::{Plugin, PluginBuildError, PluginFactory, PluginPhase};

use super::{DEFAULT_CONFIG, JwtConfig, JwtPlugin};
use crate::{build_failure, compile::credentials, spec::JwtSpec};

/// Loads the `jwt` kind.
pub(crate) struct JwtFactory;

impl PluginFactory for JwtFactory {
    fn kind(&self) -> &'static str {
        "jwt"
    }

    fn phase(&self) -> PluginPhase {
        // A token is what names the caller, so this runs before anything that charges a quota.
        PluginPhase::Authenticate
    }

    fn default_config(&self) -> &'static str {
        DEFAULT_CONFIG
    }

    fn build(&self, spec: &PluginSpec) -> Result<Arc<dyn Plugin>, PluginBuildError> {
        let name = spec.name();
        let config = JwtConfig::from_spec(spec).map_err(|error| build_failure(name, error))?;
        let compiled =
            JwtSpec::compile(spec, &config).map_err(|error| build_failure(name, error))?;
        let secrets = credentials::materialize_all(name, compiled.credentials())
            .map_err(|error| build_failure(name, error))?;
        let plugin =
            JwtPlugin::new(&compiled, secrets).map_err(|error| build_failure(name, error))?;
        Ok(Arc::new(plugin))
    }
}
