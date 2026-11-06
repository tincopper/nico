//! The LLM plan one document compiles into, and the runtime that compiles it.
//!
//! The request path reaches an AI route through a byte-level seam owned by `nico-http`
//! ([`nico_http::llm::LlmRoutes`]): a destination, a path, header pairs, the bytes to send and a
//! per-response translator. This module is the crate that answers it — the process assembly point
//! where a client codec, a provider adapter, the route facts and a materialized credential become one
//! front-end per provider route.
//!
//! # Modules
//!
//! - `credentials` — reading a `credential_ref` (`env:`/`file:`) once per publication, and the failure
//!   a reference that cannot be read produces.
//! - `error` — [`LlmBuildError`]: every reason a document's provider routes cannot be served.
//! - `plan` — [`LlmPlan`]: the compiled front-ends, keyed by route id, and the checks that decide
//!   whether they may be published.
//! - `runtime` — [`LlmRuntime`]: the registries and the resolver a plan is compiled with, created once
//!   per process and shared by every reload.
//!
//! # Where the refusals live
//!
//! Everything here is a **publication** decision. A document whose AI route cannot be served is
//! refused while it is compiled, so the previously published generation keeps serving and a caller
//! never receives a prompt-shaped answer to a configuration mistake. What a *request* can still be
//! refused for — an unsupported capability, a document the codec cannot read, a provider refusal — is
//! the front-end's, and it keeps the client protocol's own error document.
//!
//! Two refusals matter most because they are the ones an operator is most likely to write by accident:
//! a plugin asking for the request body on a converted route (two buffering contracts, one body), and
//! a `passthrough` route whose two sides do not speak the same protocol (the client's bytes would reach
//! a peer that parses something else). Both are refused before anything is published.

mod credentials;
mod error;
mod plan;
mod runtime;

#[cfg(test)]
mod tests;

pub use credentials::{CredentialError, CredentialResolver, EnvFileCredentials};
pub use error::LlmBuildError;
pub use plan::LlmPlan;
pub use runtime::LlmRuntime;
