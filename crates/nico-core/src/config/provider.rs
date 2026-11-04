//! AI providers a route can forward to.
//!
//! A [`Provider`] names a vendor endpoint and the *reference* to the credential used to call it.
//! The credential itself never appears in the document, and this module never resolves it: reading
//! `env:`/`file:` references belongs to the data plane that makes the call.

use serde::Deserialize;

/// The vendor protocol a provider speaks.
///
/// The set is closed for this milestone but may grow, so matches from other crates must be
/// exhaustive over the variants they know plus a wildcard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ProviderKind {
    /// OpenAI-compatible HTTP API.
    OpenAi,
}

/// An AI provider endpoint referenced by a [`Route`](super::Route).
///
/// Fields are private and there is no public constructor: a `Provider` only exists inside a
/// [`Document`](super::Document) that has passed reference and semantic validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provider {
    pub(super) name: String,
    pub(super) kind: ProviderKind,
    pub(super) base_url: String,
    pub(super) credential_ref: String,
    pub(super) model_allowlist: Vec<String>,
    pub(super) timeout_ms: Option<u32>,
}

impl Provider {
    /// Returns the unique provider name referenced by routes.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the vendor protocol.
    #[must_use]
    pub const fn kind(&self) -> ProviderKind {
        self.kind
    }

    /// Returns the upstream base URL, for example `https://api.openai.com`.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Returns the credential reference, for example `env:OPENAI_API_KEY`.
    ///
    /// The value is a reference, not a secret: it is never resolved here.
    #[must_use]
    pub fn credential_ref(&self) -> &str {
        &self.credential_ref
    }

    /// Returns the models this provider may serve; empty means "no restriction".
    #[must_use]
    pub fn model_allowlist(&self) -> &[String] {
        &self.model_allowlist
    }

    /// Returns the provider-level timeout in milliseconds, when the document sets one.
    #[must_use]
    pub const fn timeout_ms(&self) -> Option<u32> {
        self.timeout_ms
    }
}
