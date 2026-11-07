//! LLM providers a route can forward to.
//!
//! A [`Provider`] names a vendor endpoint and the *reference* to the credential used to call it.
//! The credential itself never appears in the document, and this module never resolves it: reading
//! `env:`/`file:` references belongs to the data plane that makes the call.
//!
//! It also carries the logical → physical model map. The route forwards the name the caller wrote;
//! which name the upstream is asked for is a property of the endpoint, so the map lives here rather
//! than on the route. Two endpoints that need different parameters are two provider entries.

use std::collections::BTreeMap;

/// An LLM provider endpoint referenced by a [`Route`](super::Route).
///
/// Fields are private and there is no public constructor: a `Provider` only exists inside a
/// [`Document`](super::Document) that has passed reference and semantic validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provider {
    pub(super) name: String,
    /// The adapter registry's key, exactly as the document spelled it.
    ///
    /// An opaque token, not an enum: which kinds exist is the adapter registry's answer, so adding a
    /// provider is a new module in `nico-llm` and a registration, never a change here. The spelling is
    /// validated for shape by [`semantic`](super::semantic) and for existence at publication.
    pub(super) kind: String,
    pub(super) base_url: String,
    pub(super) credential_ref: String,
    pub(super) model_allowlist: Vec<String>,
    pub(super) model_map: BTreeMap<String, String>,
    pub(super) timeout_ms: Option<u32>,
}

impl Provider {
    /// Returns the unique provider name referenced by routes.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the adapter registry's key this provider asks for, for example `openai`.
    ///
    /// Reported in the request record as `llm.provider` and used for one registry lookup. Nothing
    /// derives vocabulary from it: this crate cannot say which kinds exist.
    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
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
    ///
    /// The names are the **provider's** (`gpt-4o`), not the aliases a caller writes: `model_map`
    /// resolves an alias to a physical name, and this list is the operator's statement about which of
    /// those the endpoint may reach. The check runs on the resolved name while the request is prepared,
    /// so a model this list does not name is refused before anything is encoded — the field is a
    /// control, not a comment, and a document that sets it changes what the route will serve.
    #[must_use]
    pub fn model_allowlist(&self) -> &[String] {
        &self.model_allowlist
    }

    /// Returns the logical → physical model map, empty when the document declares none.
    ///
    /// A caller writes the left-hand name; the provider adapter sends the right-hand name. An empty
    /// map means the caller's name is sent unchanged. The map renames only: anything else that
    /// differs between two endpoints (base URL, credential, timeouts) is a second provider entry,
    /// because those are properties of the endpoint rather than of one request.
    #[must_use]
    pub fn model_map(&self) -> &BTreeMap<String, String> {
        &self.model_map
    }

    /// Returns the provider-level timeout in milliseconds, when the document sets one.
    #[must_use]
    pub const fn timeout_ms(&self) -> Option<u32> {
        self.timeout_ms
    }
}
