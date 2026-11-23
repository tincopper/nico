//! The `wasm` kind: a WASM artifact from the document, compiled at publication and run in the
//! `nico-wasm` sandbox at request time.
//!
//! The configuration names the artifact, the sandbox budget, and the refusal the module's
//! `"refuse"` action produces. Everything else is the built-in plugins' own shape: the same
//! `PluginSpec`, the same `Rejection` compilation, the same `PluginPhase` ordering, the same
//! `Outcome`s. A document that names `kind: wasm` is refused exactly like one that names an unknown
//! kind when the build carries no WASM host.

use std::{path::PathBuf, sync::Arc};

use nico_core::{ErrorCategory, PluginSpec};
use nico_http::{
    Outcome, Plugin, PluginBuildError, PluginError, PluginFactory, PluginInfo, PluginPhase,
    PluginResult, Rejection, ResponseData, plugin::PluginContext,
};
use nico_wasm::{RequestFacts, ResponseFacts, WasmPlugin, WasmResources};

use crate::rejection;

/// The `kind` the registry answers to.
const KIND: &str = "wasm";

/// The stable code of a WASM plugin's refusal.
const REFUSED_CODE: &str = "plugin_refused";

/// The fixed message of a WASM plugin's refusal.
const REFUSED_MESSAGE: &str = "the request was refused by a plugin";

/// The log-only detail of a WASM plugin's refusal.
const REFUSED_DETAIL: &str = "the module answered refuse";

/// The sandbox budget a document gets when it names neither value.
const DEFAULT_FUEL: u64 = 10_000_000;

/// The memory ceiling a document gets when it names none, in WebAssembly pages.
const DEFAULT_MEMORY_PAGES: u32 = 16;

/// One WASM plugin instance: the compiled module and the compiled refusal.
struct WasmInstance {
    name: String,
    module: Arc<WasmPlugin>,
    rejection: Arc<Rejection>,
}

impl WasmInstance {
    /// Builds the refusal the module's `"refuse"` action answers with.
    fn refusal(&self) -> PluginError {
        PluginError::Refused {
            rejection: Arc::clone(&self.rejection),
            detail: REFUSED_DETAIL,
            retry_after: None,
        }
    }

    /// Maps one sandbox failure onto the plugin error: the failure detail goes to the log, the
    /// error is the fixed sentence the data plane maps to a `500`.
    fn execute_failed(&self, error: nico_wasm::WasmPluginError) -> PluginError {
        tracing::error!(
            plugin = %self.name,
            error = %error,
            "WASM module failed in the sandbox"
        );
        PluginError::Execute("the module ran past its sandbox budget or failed")
    }

    /// Maps one module response onto the plugin outcome.
    fn map_action(&self, action: nico_wasm::Action) -> PluginResult {
        match action {
            nico_wasm::Action::Refuse => Err(self.refusal()),
            nico_wasm::Action::Continue => Ok(Outcome::Continue),
            nico_wasm::Action::Respond { status, body, .. } => Ok(Outcome::Respond(
                ResponseData::new(status, Vec::new(), body.into_bytes())
                    .map_err(|_| PluginError::Execute("the module wrote an unusable response"))?,
            )),
        }
    }
}

#[async_trait::async_trait]
impl Plugin for WasmInstance {
    fn name(&self) -> &str {
        &self.name
    }

    fn info(&self) -> PluginInfo {
        PluginInfo::new(
            "1",
            "a WASM module loaded from the artifact path the document names",
            "{}",
        )
    }

    async fn on_request(&self, ctx: &mut dyn PluginContext) -> PluginResult {
        // The context offers per-name reads, not the whole map: the facts the module sees are the
        // well-known request headers, read through the same accessor a built-in plugin uses.
        let headers: Vec<(String, String)> = [
            "content-type",
            "authorization",
            "x-request-id",
            "user-agent",
            "accept",
        ]
        .iter()
        .filter_map(|name| {
            ctx.request_header(name)
                .transpose()
                .map(|value| value.map(|value| ((*name).to_owned(), value)))
        })
        .collect::<Result<_, _>>()?;
        let facts = RequestFacts {
            method: &ctx.method(),
            path: std::str::from_utf8(ctx.uri()).unwrap_or(""),
            headers: &headers,
            subject: ctx.identity(),
        };
        let action = self
            .module
            .on_request(&facts, &serde_json::json!({ "subject": facts.subject }))
            .map_err(|error| self.execute_failed(error))?;
        self.map_action(action)
    }

    async fn on_response(&self, ctx: &mut dyn PluginContext) -> PluginResult {
        let status = ctx.status()?;
        let facts = ResponseFacts {
            status,
            headers: &[],
        };
        let action = self
            .module
            .on_response(&facts)
            .map_err(|error| self.execute_failed(error))?;
        self.map_action(action)
    }

    async fn on_logging(&self, _ctx: &mut dyn PluginContext) {
        // The module's logging export runs, but its action is never read: the request is answered
        // by the time this stage runs.
        let _ = self.module.on_logging();
    }
}

/// The `kind: wasm` factory: reads the artifact at publication, compiles it once, hands the
/// instance the compiled refusal and the sandbox budget.
pub struct WasmFactory;

impl PluginFactory for WasmFactory {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn phase(&self) -> PluginPhase {
        PluginPhase::Other
    }

    fn default_config(&self) -> &'static str {
        r#"{"module": "./plugins/filter.wasm", "refuse_status": 403, "fuel": 10000000, "memory_pages": 16}"#
    }

    fn build(&self, spec: &PluginSpec) -> Result<Arc<dyn Plugin>, PluginBuildError> {
        let config = WasmConfig::from_spec(spec)
            .map_err(|error| PluginBuildError::new(spec.name(), error.to_string()))?;
        let artifact = std::fs::read(&config.module).map_err(|source| {
            PluginBuildError::new(
                spec.name(),
                format!("`plugins[].config.module` could not be read: {source}"),
            )
        })?;
        let module = WasmPlugin::compile(&artifact, config.resources())
            .map_err(|error| PluginBuildError::new(spec.name(), error.to_string()))?;
        let rejection = Arc::new(
            rejection::compile(
                spec.name(),
                REFUSED_CODE,
                REFUSED_MESSAGE,
                config.refuse_status,
                ErrorCategory::Client,
                None,
                spec.error_response(),
            )
            .map_err(|error| PluginBuildError::new(spec.name(), error.to_string()))?,
        );
        Ok(Arc::new(WasmInstance {
            name: spec.name().to_owned(),
            module: Arc::new(module),
            rejection,
        }))
    }
}

/// The `kind: wasm` configuration: what the document declares beyond the plugin envelope.
struct WasmConfig {
    module: PathBuf,
    refuse_status: u16,
    fuel: u64,
    memory_pages: u32,
}

impl WasmConfig {
    /// Reads the configuration from the spec's mapping value.
    ///
    /// # Errors
    /// Names the field at fault: a missing `module`, a non-mapping config, or a budget that is not
    /// a positive number.
    fn from_spec(spec: &PluginSpec) -> Result<Self, PluginBuildError> {
        let fail = |reason: String| PluginBuildError::new(spec.name(), reason);
        let object = spec
            .config()
            .as_mapping()
            .ok_or_else(|| fail("`plugins[].config` must be a mapping".to_owned()))?;
        let module = object
            .get("module")
            .and_then(|value| value.as_str())
            .map(PathBuf::from)
            .ok_or_else(|| {
                fail(
                    "`plugins[].config.module` is required: the path of the WASM artifact"
                        .to_owned(),
                )
            })?;
        let refuse_status = object
            .get("refuse_status")
            .map(|value| {
                value.as_u64().map(|value| value as u16).ok_or_else(|| {
                    fail("`plugins[].config.refuse_status` must be a number".to_owned())
                })
            })
            .transpose()?
            .unwrap_or(403);
        let fuel = object
            .get("fuel")
            .map(|value| {
                value.as_u64().filter(|fuel| *fuel > 0).ok_or_else(|| {
                    fail("`plugins[].config.fuel` must be a positive instruction count".to_owned())
                })
            })
            .transpose()?
            .unwrap_or(DEFAULT_FUEL);
        let memory_pages = object
            .get("memory_pages")
            .map(|value| {
                value
                    .as_u64()
                    .map(|pages| pages as u32)
                    .filter(|pages| *pages > 0)
                    .ok_or_else(|| {
                        fail(
                            "`plugins[].config.memory_pages` must be a positive page count"
                                .to_owned(),
                        )
                    })
            })
            .transpose()?
            .unwrap_or(DEFAULT_MEMORY_PAGES);
        Ok(Self {
            module,
            refuse_status,
            fuel,
            memory_pages,
        })
    }

    fn resources(&self) -> WasmResources {
        WasmResources {
            fuel: self.fuel,
            memory_pages: self.memory_pages,
        }
    }
}
