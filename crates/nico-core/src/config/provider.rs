//! LLM providers a route can forward to.
//!
//! A [`Provider`] names a vendor endpoint and the *reference* to the credential used to call it.
//! The credential itself never appears in the document, and this module never resolves it: reading
//! `env:`/`file:` references belongs to the data plane that makes the call.
//!
//! It also carries the logical → physical model map and the endpoint's own output ceiling. The route
//! forwards the name the caller wrote; which name the upstream is asked for, and how large an answer
//! it may produce when the caller names no limit, is a property of the endpoint, so both live here
//! rather than on the route. Two endpoints that need different parameters are two provider entries.
//!
//! The endpoint's failure budget is an endpoint property for the same reason: how many consecutive
//! failures make the gateway stop calling this endpoint, and how long it waits before probing again,
//! describe the endpoint, not any one request. Every provider carries a [`CircuitBreaker`]; the
//! document writes one explicitly to move the knobs, and `failure_threshold: 0` is the spelling for
//! "never stop calling this endpoint".

use std::collections::BTreeMap;

/// The failure count a provider's breaker tolerates when the document writes no
/// `circuit_breaker.failure_threshold`.
pub const DEFAULT_FAILURE_THRESHOLD: u32 = 5;

/// The cooldown in milliseconds an open breaker waits before probing again, when the document
/// writes no `circuit_breaker.cooldown_ms`.
pub const DEFAULT_COOLDOWN_MS: u32 = 30_000;

/// One provider endpoint's circuit-breaker parameters.
///
/// A breaker opens after `failure_threshold` consecutive failures to the endpoint and, once open,
/// admits one probe every `cooldown_ms` to learn whether it recovered. `failure_threshold` of `0`
/// disables the breaker for the provider: the failures are still counted and logged, but no request
/// is ever skipped because of them. The values travel with the provider's identity when its runtime
/// state is carried across generations, so re-tuning them starts the breaker over — an inheritance
/// rule that keeps "what the state machine knows" equal to "what the document says".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CircuitBreaker {
    failure_threshold: u32,
    cooldown_ms: u32,
}

impl CircuitBreaker {
    /// Builds breaker parameters from already validated values.
    pub(super) const fn new(failure_threshold: u32, cooldown_ms: u32) -> Self {
        Self {
            failure_threshold,
            cooldown_ms,
        }
    }

    /// Returns the consecutive failures that open the breaker; `0` means the breaker is disabled.
    #[must_use]
    pub const fn failure_threshold(&self) -> u32 {
        self.failure_threshold
    }

    /// Returns how long an open breaker stays open before it admits one probe.
    #[must_use]
    pub const fn cooldown_ms(&self) -> u32 {
        self.cooldown_ms
    }
}

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
    pub(super) max_output_tokens: Option<u32>,
    pub(super) circuit_breaker: CircuitBreaker,
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
    /// differs between two endpoints (base URL, credential) is a second provider entry, because
    /// those are properties of the endpoint rather than of one request.
    #[must_use]
    pub fn model_map(&self) -> &BTreeMap<String, String> {
        &self.model_map
    }

    /// Returns the output ceiling this endpoint uses when the request names none.
    ///
    /// The value is endpoint policy, not a default the gateway invents: a dialect that requires an
    /// output ceiling cannot be served from a request that omits one unless the route answers for it,
    /// and a constant would silently cap a caller who named nothing. `None` means the endpoint
    /// imposes none, and a request's own limit always takes precedence. Zero is refused while the
    /// document is validated, so "set, but not a ceiling" cannot be spelled.
    #[must_use]
    pub const fn max_output_tokens(&self) -> Option<u32> {
        self.max_output_tokens
    }

    /// Returns the endpoint's circuit-breaker parameters.
    ///
    /// Every provider carries a value: an entry that writes no `circuit_breaker` section takes the
    /// built-in defaults, and `failure_threshold` of `0` means the breaker is disabled for it.
    #[must_use]
    pub const fn circuit_breaker(&self) -> &CircuitBreaker {
        &self.circuit_breaker
    }
}
