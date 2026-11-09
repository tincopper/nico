//! One `routes` entry of a fixture document.
//!
//! A route is the unit a document's policy is attached to, so the builder states the parts a scenario
//! varies: the target, the host and prefix, the two resource bounds, the client protocol and mode of a
//! provider route, and the plugins it runs. The rendering lives with the document, because a field is
//! only valid next to the target that owns it.

/// What a route forwards to: exactly one of the two, like the document itself.
#[derive(Debug, Clone)]
pub(super) enum Target {
    Upstream(String),
    Provider(String),
}

/// One `routes` entry of a fixture document.
#[derive(Debug, Clone)]
pub(crate) struct Route {
    pub(super) id: String,
    pub(super) host: Option<String>,
    pub(super) path_prefix: String,
    pub(super) target: Target,
    pub(super) timeout_ms: Option<u32>,
    pub(super) max_body_bytes: Option<u32>,
    pub(super) max_response_bytes: Option<u32>,
    /// The client protocol a provider route serves; `None` for an upstream route.
    pub(super) protocol: Option<String>,
    /// The provider-route mode; `None` leaves the document's `convert` default.
    pub(super) mode: Option<String>,
    pub(super) plugins: Vec<String>,
    /// Attempts the route may absorb from one target; `None` leaves the document default of `1`.
    pub(super) retry_budget: Option<u8>,
    /// The fallback chain; `None` leaves no chain. A scenario writes names the document declares.
    pub(super) fallback: Option<Vec<String>>,
}

impl Route {
    /// A route for every host that forwards `path_prefix` to an upstream.
    pub(crate) fn new(id: &str, path_prefix: &str, upstream: &str) -> Self {
        Self {
            id: id.to_owned(),
            host: None,
            path_prefix: path_prefix.to_owned(),
            target: Target::Upstream(upstream.to_owned()),
            timeout_ms: None,
            max_body_bytes: None,
            max_response_bytes: None,
            protocol: None,
            mode: None,
            plugins: Vec::new(),
            retry_budget: None,
            fallback: None,
        }
    }

    /// A route that forwards `path_prefix` to a provider, serving `openai.chat` by default.
    pub(crate) fn provider(id: &str, path_prefix: &str, provider: &str) -> Self {
        Self {
            id: id.to_owned(),
            host: None,
            path_prefix: path_prefix.to_owned(),
            target: Target::Provider(provider.to_owned()),
            timeout_ms: None,
            max_body_bytes: None,
            max_response_bytes: None,
            protocol: Some("openai.chat".to_owned()),
            mode: None,
            plugins: Vec::new(),
            retry_budget: None,
            fallback: None,
        }
    }

    /// Restricts the route to one exact `Host`.
    pub(crate) fn host(mut self, host: &str) -> Self {
        self.host = Some(host.to_owned());
        self
    }

    /// Adds the route-level timeout override, which may only tighten the document limit.
    pub(crate) fn timeout_ms(mut self, timeout_ms: u32) -> Self {
        self.timeout_ms = Some(timeout_ms);
        self
    }

    /// Adds the route-level body bound, which may only tighten the document limit.
    pub(crate) fn max_body_bytes(mut self, max_body_bytes: u32) -> Self {
        self.max_body_bytes = Some(max_body_bytes);
        self
    }

    /// Names the client protocol a provider route serves.
    pub(crate) fn protocol(mut self, protocol: &str) -> Self {
        self.protocol = Some(protocol.to_owned());
        self
    }

    /// Sets the provider-route mode: `convert` or `passthrough`.
    pub(crate) fn mode(mut self, mode: &str) -> Self {
        self.mode = Some(mode.to_owned());
        self
    }

    /// Adds the route-level response bound, which may only tighten the document limit.
    pub(crate) fn max_response_bytes(mut self, max_response_bytes: u32) -> Self {
        self.max_response_bytes = Some(max_response_bytes);
        self
    }

    /// Names the plugins the route runs, in the order the document lists them.
    pub(crate) fn plugins(mut self, names: &[&str]) -> Self {
        self.plugins = names.iter().map(|name| (*name).to_owned()).collect();
        self
    }

    /// Sets the route's retry budget: total attempts one target may absorb, including the first.
    pub(crate) fn retry_budget(mut self, retry_budget: u8) -> Self {
        self.retry_budget = Some(retry_budget);
        self
    }

    /// Declares the fallback chain: provider names tried in order after the route's target fails.
    pub(crate) fn fallback(mut self, names: &[&str]) -> Self {
        self.fallback = Some(names.iter().map(|name| (*name).to_owned()).collect());
        self
    }
}
