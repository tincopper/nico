//! The serde shape of a resources document, before any validation.
//!
//! These types are the only place the YAML spelling lives. They never leave the `config` module:
//! [`Document`](super::Document) stays a validated domain type with private fields, so a caller
//! cannot build one by deserializing YAML. Every struct denies unknown fields and none of them uses
//! `serde(default)` at document level — a missing section is an error, not an empty one, because a
//! truncated document must not look like a working configuration.

use serde::Deserialize;

use super::{provider::ProviderKind, upstream::Scheme};

/// The document exactly as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawDocument {
    #[serde(rename = "apiVersion")]
    pub(super) api_version: String,
    pub(super) limits: RawLimits,
    pub(super) providers: Vec<RawProvider>,
    pub(super) upstreams: Vec<RawUpstream>,
    pub(super) routes: Vec<RawRoute>,
}

/// The `limits` section exactly as written, before range validation.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawLimits {
    pub(super) max_request_body_bytes: u32,
    pub(super) request_timeout_ms: u32,
}

/// One `providers` entry exactly as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawProvider {
    pub(super) name: String,
    pub(super) kind: ProviderKind,
    pub(super) base_url: String,
    pub(super) credential_ref: String,
    /// `None` and `[]` mean the same thing — no model restriction — so the empty case does not have
    /// to be written out; the field is still rejected when it contains the wrong type.
    pub(super) model_allowlist: Option<Vec<String>>,
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
}
