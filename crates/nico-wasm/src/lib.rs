//! The WASM plugin host: a wasmi sandbox that turns plugin modules into nico plugins.
//!
//! A module is compiled **at publication** (a missing or invalid artifact is a publication failure,
//! exactly like an unreadable certificate), so the request path only instantiates a prepared engine
//! and calls one export per stage. Between the gateway and the module there is one ABI: the host
//! writes the stage's facts as JSON into linear memory, calls the export, and reads back an action
//! (`continue` / `refuse` / `respond`). No WASI, no clock, no network, no files — what a plugin can
//! see is the bytes the host hands it, which is why the sandbox boundary is the ABI's shape rather
//! than a runtime switch.
//!
//! Every call is bounded twice, by values the document declared: **fuel** (wasmi's instruction
//! metering) and **memory pages**. Exhausting either, or trapping, maps to the plugin system's
//! execution-failure path — the process stays alive, the request is answered `500`, and the next
//! request starts clean.
//!
//! The stage set this milestone hosts is the non-streaming one: `on_request`, `on_response` and
//! `on_logging`. An export a module does not provide means that stage continues unchanged — the same
//! rule a built-in plugin's phase declaration follows.

#![deny(missing_docs)]

pub mod action;
pub mod facts;
pub mod plugin;

pub use action::Action;
pub use facts::{RequestFacts, ResponseFacts};
pub use plugin::{WasmPlugin, WasmPluginError, WasmResources};
