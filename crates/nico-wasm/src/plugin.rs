//! One WASM module as a nico plugin: the wasmi store, the stage calls, and the budget each call
//! runs under.

use std::sync::Arc;

use wasmi::{Engine, Linker, Module, Store};

use crate::{
    action::{self, Action, ActionError},
    facts::{RequestFacts, ResponseFacts},
};

/// Why one stage call could not run or be read.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WasmPluginError {
    /// The artifact is not a WebAssembly module.
    #[error("the module does not compile: {message}")]
    Compile {
        /// Why wasmi refused the artifact, for the publication log.
        message: String,
    },
    /// The stage's export is missing, or does not have the ABI's signature.
    #[error("the export `{export}` is missing or has the wrong signature")]
    Export {
        /// The export name the stage asked for.
        export: &'static str,
    },
    /// The module exhausted its fuel, its memory, or trapped.
    #[error("the module ran past its sandbox budget: {message}")]
    Budget {
        /// What ran out, for the log.
        message: String,
    },
    /// The module's output was not a readable action.
    #[error("{0}")]
    Action(#[from] ActionError),
}

/// The resources one module call may spend, declared by the document.
#[derive(Debug, Clone, Copy)]
pub struct WasmResources {
    /// wasmi fuel per stage call: the instruction meter the sandbox runs under.
    pub fuel: u64,
    /// The module's memory ceiling, in WebAssembly pages (64 KiB each).
    pub memory_pages: u32,
}

impl WasmResources {
    /// The default a document gets when it names neither: 10 million instructions and 1 MiB.
    #[must_use]
    pub const fn default() -> Self {
        Self {
            fuel: 10_000_000,
            memory_pages: 16,
        }
    }
}

/// One compiled module plus its configuration, prepared at publication.
///
/// The [`Module`] and [`Engine`] are publication facts — compile once, share everywhere. An
/// An `Instance` is deliberately **not** kept: wasmi instances belong to the store they were created
/// in, and one call's fuel spend must not carry into the next, so every call instantiates its own
/// instance in its own store. Instantiation is the sandbox's per-call setup, and fuel metering
/// charges it.
#[derive(Debug, Clone)]
pub struct WasmPlugin {
    engine: Arc<Engine>,
    module: Arc<Module>,
    resources: WasmResources,
}

impl WasmPlugin {
    /// Compiles the artifact and instantiates it once per generation.
    ///
    /// The engine is per plugin, not per process: a module's fuel accounting lives in the store, and
    /// one module exhausting its budget must not touch another's. Compilation failure is the
    /// publication's problem, not a request-time one.
    ///
    /// # Errors
    /// Returns [`WasmPluginError::Compile`] when `artifact` is not a WebAssembly module wasmi can
    /// load.
    pub fn compile(artifact: &[u8], resources: WasmResources) -> Result<Self, WasmPluginError> {
        // Fuel metering is the sandbox's CPU budget: enabled per engine, charged per call below.
        let mut config = wasmi::Config::default();
        config.consume_fuel(true);
        let engine = Engine::new(&config);
        let module = Module::new(&engine, artifact).map_err(|error| WasmPluginError::Compile {
            message: error.to_string(),
        })?;
        Ok(Self {
            engine: Arc::new(engine),
            module: Arc::new(module),
            resources,
        })
    }

    /// Runs the `on_request` export over the stage's facts.
    ///
    /// The ABI's one overlap rule: the host writes the input at the pointer `nico_alloc` returned,
    /// so the module's output must live elsewhere — an output that aliases the input buffer reads
    /// back as the input, not as an action.
    ///
    /// # Errors
    /// Returns [`WasmPluginError::Export`] when the module provides no readable `on_request`, and
    /// [`WasmPluginError::Budget`] when the call exceeded its fuel or memory, or trapped.
    pub fn on_request(
        &self,
        facts: &RequestFacts<'_>,
        subject_json: &serde_json::Value,
    ) -> Result<Action, WasmPluginError> {
        let input = serde_json::json!({
            "method": facts.method,
            "path": facts.path,
            "headers": facts.headers,
            "subject": subject_json,
        });
        self.call("on_request", &input)
    }

    /// Runs the `on_response` export over the upstream's head.
    ///
    /// # Errors
    /// The same failures as [`Self::on_request`], under the `on_response` export.
    pub fn on_response(&self, facts: &ResponseFacts<'_>) -> Result<Action, WasmPluginError> {
        let input = serde_json::json!({
            "status": facts.status,
            "headers": facts.headers,
        });
        self.call("on_response", &input)
    }

    /// Runs the `on_logging` export; its action is never read.
    ///
    /// # Errors
    /// The same failures as [`Self::on_request`], under the `on_logging` export — but the caller
    /// ignores them beyond a log line, because the request is already answered.
    pub fn on_logging(&self) -> Result<Action, WasmPluginError> {
        self.call("on_logging", &serde_json::json!({}))
    }

    fn call(
        &self,
        export: &'static str,
        input: &serde_json::Value,
    ) -> Result<Action, WasmPluginError> {
        // One store per call: fuel accounting and the instance's lifetime are the call's, so a
        // request can neither spend another's budget nor keep an instance alive past its answer.
        let mut store = Store::new(&self.engine, ());
        let linker = <Linker<()>>::new(&self.engine);
        store
            .set_fuel(self.resources.fuel)
            .map_err(|error| WasmPluginError::Budget {
                message: error.to_string(),
            })?;
        let instance = linker
            .instantiate_and_start(&mut store, &self.module)
            .map_err(|error| WasmPluginError::Budget {
                message: error.to_string(),
            })?;
        let memory = instance
            .get_memory(&store, "memory")
            .ok_or(WasmPluginError::Export { export })?;
        let alloc = instance
            .get_typed_func::<i32, i32>(&store, "nico_alloc")
            .map_err(|_| WasmPluginError::Export { export })?;
        let run = instance
            .get_typed_func::<(i32, i32), i64>(&store, export)
            .map_err(|_| WasmPluginError::Export { export })?;

        let bytes = serde_json::to_vec(input).map_err(|error| WasmPluginError::Budget {
            message: format!("the stage facts did not serialize: {error}"),
        })?;
        store
            .set_fuel(self.resources.fuel)
            .map_err(|error| WasmPluginError::Budget {
                message: error.to_string(),
            })?;
        let ptr = alloc.call(&mut store, bytes.len() as i32).map_err(trap)?;
        memory
            .write(&mut store, ptr as usize, &bytes)
            .map_err(|error| WasmPluginError::Budget {
                message: error.to_string(),
            })?;

        let result = run
            .call(&mut store, (ptr as i32, bytes.len() as i32))
            .map_err(trap)?;
        let out_ptr = (result >> 32) as u32 as usize;
        let out_len = (result & 0xFFFF_FFFF) as u32 as usize;
        // A module that returns no coordinates has nothing to say: the implicit answer for any
        // stage is "continue", so the host does not force every module to write a JSON document
        // for the common path.
        if out_len == 0 {
            return Ok(Action::Continue);
        }
        let mut output = vec![0u8; out_len];
        memory
            .read(&store, out_ptr, &mut output)
            .map_err(|error| WasmPluginError::Budget {
                message: error.to_string(),
            })?;
        action::decode(&output).map_err(WasmPluginError::from)
    }
}

/// Maps one wasmi trap onto the budget error: whatever the module did, it ran past its sandbox.
fn trap(error: wasmi::Error) -> WasmPluginError {
    WasmPluginError::Budget {
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests;
