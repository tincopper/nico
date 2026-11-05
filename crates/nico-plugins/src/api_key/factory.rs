//! The `api_key` kind's factory: the one place an `api_key` entry becomes a running plugin.
//!
//! The steps are the kind's, not the document model's: read `config` with `deny_unknown_fields`,
//! compile the key set and both refusals, materialize one credential per declared key, and build the
//! plugin. A failure refuses the document and keeps the last known good generation serving.

use std::sync::Arc;

use nico_core::PluginSpec;
use nico_http::{Plugin, PluginBuildError, PluginFactory, PluginPhase};

use super::{ApiKeyConfig, ApiKeyPlugin, DEFAULT_CONFIG};
use crate::{build_failure, compile::credentials, spec::ApiKeySpec};

/// Loads the `api_key` kind.
pub(crate) struct ApiKeyFactory;

impl PluginFactory for ApiKeyFactory {
    fn kind(&self) -> &'static str {
        "api_key"
    }

    fn phase(&self) -> PluginPhase {
        // A key is what names the caller, so this runs before anything that charges a quota.
        PluginPhase::Authenticate
    }

    fn default_config(&self) -> &'static str {
        DEFAULT_CONFIG
    }

    fn build(&self, spec: &PluginSpec) -> Result<Arc<dyn Plugin>, PluginBuildError> {
        let name = spec.name();
        let config = ApiKeyConfig::from_spec(spec).map_err(|error| build_failure(name, error))?;
        let compiled =
            ApiKeySpec::compile(spec, &config).map_err(|error| build_failure(name, error))?;
        let secrets = credentials::materialize_all(name, compiled.credentials())
            .map_err(|error| build_failure(name, error))?;
        let plugin =
            ApiKeyPlugin::new(&compiled, secrets).map_err(|error| build_failure(name, error))?;
        Ok(Arc::new(plugin))
    }
}
