//! The serde shape of a resources document, before any validation.
//!
//! These types are the only place the YAML spelling lives. They never leave the `config` module:
//! [`Document`](super::Document) stays a validated domain type with private fields, so a caller
//! cannot build one by deserializing YAML. Every struct denies unknown fields and none of them uses
//! `serde(default)` at document level — a missing section is an error, not an empty one, because a
//! truncated document must not look like a working configuration.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::{route::RouteMode, upstream::Scheme};

/// The document exactly as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawDocument {
    #[serde(rename = "apiVersion")]
    pub(super) api_version: String,
    pub(super) request: RawRequest,
    pub(super) providers: Vec<RawProvider>,
    pub(super) upstreams: Vec<RawUpstream>,
    pub(super) routes: Vec<RawRoute>,
    pub(super) plugins: Vec<RawPlugin>,
    /// Names of declared plugins that run for every request, before routing.
    ///
    /// Absent and `[]` mean the same thing — no global policy — so a document that only uses
    /// route-scoped plugins does not have to spell the empty list out.
    #[serde(default)]
    pub(super) global_plugins: Vec<String>,
}

/// The `request` section exactly as written, before range validation.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawRequest {
    pub(super) max_body_bytes: u32,
    /// Absent means the 8 MiB default; the field exists so a document can lower it, not so every
    /// document has to restate it. An `Option` already means "absent", so no `serde` default is
    /// needed — and a strict document type is not where a default belongs.
    pub(super) max_response_bytes: Option<u32>,
    pub(super) timeout_ms: u32,
}

/// One `providers` entry exactly as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawProvider {
    pub(super) name: String,
    pub(super) kind: String,
    pub(super) base_url: String,
    pub(super) credential_ref: String,
    /// `None` and `[]` mean the same thing — no model restriction — so the empty case does not have
    /// to be written out; the field is still rejected when it contains the wrong type. Its entries are
    /// the provider's own model names, enforced on the resolved name while a request is prepared.
    pub(super) model_allowlist: Option<Vec<String>>,
    /// Logical model name → the name this provider expects. `None` and `{}` mean the same thing —
    /// send the caller's name unchanged — so an unmapped provider does not have to write an empty
    /// mapping out.
    pub(super) model_map: Option<BTreeMap<String, String>>,
    pub(super) timeout_ms: Option<u32>,
}

/// One `upstreams` entry exactly as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawUpstream {
    pub(super) name: String,
    pub(super) scheme: Scheme,
    pub(super) host: String,
    pub(super) port: u16,
}

/// One `routes` entry exactly as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawRoute {
    pub(super) id: String,
    pub(super) host: Option<String>,
    pub(super) path_prefix: String,
    /// Exactly one of `upstream` and `provider` must be present; the pair is turned into a
    /// [`RouteTarget`](super::RouteTarget) during validation so both mistakes get their own error.
    pub(super) upstream: Option<String>,
    pub(super) provider: Option<String>,
    pub(super) timeout_ms: Option<u32>,
    pub(super) max_body_bytes: Option<u32>,
    /// The client protocol a provider route converts, kept as written: whether a codec exists for it
    /// is a publication decision of the crate that owns the registry, not a document rule.
    ///
    /// Required on a provider route and refused on an upstream one, so the pair is checked where the
    /// target is resolved rather than here.
    pub(super) protocol: Option<String>,
    /// Absent means [`RouteMode::Convert`].
    pub(super) mode: Option<RouteMode>,
    /// Only a provider route has a response to convert, so this is refused on an upstream one.
    pub(super) max_response_bytes: Option<u32>,
    /// Plugin names this route runs, in document order. Absent and `[]` mean the same thing — no
    /// plugins — so a route without a policy does not have to spell the empty list out.
    pub(super) plugins: Option<Vec<String>>,
}

/// One `plugins` entry exactly as written.
///
/// The entry is an **envelope**: who it is (`name`), which implementation runs it (`kind`), whether it
/// is live, whether it buffers the request body, how it refuses a caller, and a `config` value this
/// model does not interpret. Every field but `config` is common to all plugins, so the envelope is
/// owned here; what `config` may contain belongs to the crate that implements the kind, which reads it
/// with `deny_unknown_fields` while the document is compiled. That is why the entry is flat and typed
/// instead of a flattened enum per kind: adding a kind changes no document model.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawPlugin {
    pub(super) name: String,
    pub(super) kind: String,
    #[serde(default = "enabled_by_default")]
    pub(super) enabled: bool,
    #[serde(default)]
    pub(super) buffers_body: bool,
    pub(super) error_response: Option<RawErrorResponse>,
    /// The kind's own configuration, kept exactly as written.
    ///
    /// `Value` rather than a typed enum: the document model checks the envelope and nothing else, so
    /// a new kind is a new crate rather than a new variant here. An entry that writes no `config`
    /// carries `Value::Null`, which the owning crate reads as "no fields set".
    #[serde(default)]
    pub(super) config: serde_norway::Value,
}

/// The default of `plugins[].enabled`: a plugin is live unless the document stops it.
///
/// Stopping one is why the field exists: an operator can take an entry out of service without
/// editing every route that names it.
const fn enabled_by_default() -> bool {
    true
}

/// One plugin's rejection presentation exactly as written.
///
/// It stays part of the envelope because refusing a caller is common to every plugin that can refuse:
/// the status, the content type and the body shape do not depend on the kind.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawErrorResponse {
    pub(super) status: Option<u16>,
    pub(super) content_type: Option<String>,
    /// Any YAML value: whether it must be structured or a literal string is decided by the effective
    /// content type when the entry is built, so the raw shape does not have to guess it.
    pub(super) body: Option<serde_norway::Value>,
}
