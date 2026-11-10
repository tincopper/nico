//! The self-reporting capability layer: what a probe or a scraper is told about this process.
//!
//! This crate answers one question — "what should the self-reporting surface return?" — and
//! nothing else. It installs the global metrics recorder once, renders Prometheus text, and
//! projects the configuration state into a readiness body. It does not listen on a socket, does
//! not build a runtime, and does not name an HTTP framework: the transport that binds a port and
//! reads a connection lives in `nico-server`, which hands over a method and a path and writes back
//! the [`Response`] this crate produced.
//!
//! The split is what makes the layer testable without a socket and what keeps the Prometheus
//! exporter out of the data plane's dependency tree: `nico-http` depends on the `metrics` facade
//! alone, and this crate — the only workspace member that names the exporter — depends on
//! `nico-core` and nothing else.
//!
//! # Invariants
//!
//! - **One recorder per process.** [`install`] is idempotent: the first call installs the global
//!   recorder, and every later call returns the same handle. A second installation is a startup
//!   error rather than a silent empty surface.
//! - **The readiness predicate is the caller's.** This crate receives a `bool` and a
//!   [`nico_core::ConfigStatus`]; it cannot read the published state itself, so "can this process
//!   serve" has exactly one definition, in the data plane.
//! - **Upkeep runs at scrape time.** `install_recorder()` starts no background task, and a
//!   histogram's samples reach the renderer only through `run_upkeep()`. [`Metrics::render`] runs
//!   it before rendering, so no task is owned and no histogram renders empty.
//! - **The surface is read-only.** No endpoint mutates shared state, so a probe can never change
//!   what the gateway serves.
#![deny(missing_docs)]

mod health;
mod recorder;
mod response;

pub use health::Readiness;
pub use recorder::{Metrics, MetricsError, install};
pub use response::{LIVE_PATH, METRICS_PATH, READY_PATH, Response};
