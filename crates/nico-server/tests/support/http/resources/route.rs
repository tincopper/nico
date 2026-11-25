//! One `routes` entry of a fixture document.
//!
//! A route is the unit a document's policy is attached to, so the builder states the parts a scenario
//! varies: the target, the host and prefix, the two resource bounds, the client protocol and mode of a
//! provider route, and the plugins it runs. The rendering lives with the document, because a field is
//! only valid next to the target that owns it.

mod proxy;

/// What a route forwards to: exactly one of the two, like the document itself.
#[derive(Debug, Clone)]
pub(super) enum Target {
    Upstream(String),
    Provider(String),
}

#[derive(Debug, Clone)]
pub(crate) struct Route {
    pub(super) id: String,
    pub(super) host: Option<String>,
    pub(super) path_prefix: String,
    pub(super) target: Target,
    pub(super) timeout_ms: Option<u32>,
    pub(super) max_body_bytes: Option<u32>,
    pub(super) max_response_bytes: Option<u32>,
    pub(super) protocol: Option<String>,
    pub(super) mode: Option<String>,
    pub(super) plugins: Vec<String>,
    pub(super) retry_budget: Option<u8>,
    pub(super) fallback: Option<Vec<String>>,
    pub(super) pool: Option<(Vec<(String, u16)>, &'static str)>,
    pub(super) upstream_pool: Option<(Vec<(String, u16)>, &'static str)>,
    pub(super) rewrite_prefix: Option<String>,
    pub(super) ttft_timeout_ms: Option<u32>,
    pub(super) proxy_buffering: Option<bool>,
}

impl Route {
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
            pool: None,
            upstream_pool: None,
            rewrite_prefix: None,
            ttft_timeout_ms: None,
            proxy_buffering: None,
        }
    }

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
            pool: None,
            upstream_pool: None,
            rewrite_prefix: None,
            ttft_timeout_ms: None,
            proxy_buffering: None,
        }
    }

    pub(crate) fn host(mut self, host: &str) -> Self {
        self.host = Some(host.to_owned());
        self
    }

    pub(crate) fn timeout_ms(mut self, timeout_ms: u32) -> Self {
        self.timeout_ms = Some(timeout_ms);
        self
    }

    pub(crate) fn max_body_bytes(mut self, max_body_bytes: u32) -> Self {
        self.max_body_bytes = Some(max_body_bytes);
        self
    }

    pub(crate) fn protocol(mut self, protocol: &str) -> Self {
        self.protocol = Some(protocol.to_owned());
        self
    }

    pub(crate) fn mode(mut self, mode: &str) -> Self {
        self.mode = Some(mode.to_owned());
        self
    }

    pub(crate) fn max_response_bytes(mut self, max_response_bytes: u32) -> Self {
        self.max_response_bytes = Some(max_response_bytes);
        self
    }

    pub(crate) fn plugins(mut self, names: &[&str]) -> Self {
        self.plugins = names.iter().map(|name| (*name).to_owned()).collect();
        self
    }

    pub(crate) fn retry_budget(mut self, retry_budget: u8) -> Self {
        self.retry_budget = Some(retry_budget);
        self
    }

    pub(crate) fn ttft_timeout_ms(mut self, ttft_timeout_ms: u32) -> Self {
        self.ttft_timeout_ms = Some(ttft_timeout_ms);
        self
    }

    pub(crate) fn fallback(mut self, names: &[&str]) -> Self {
        self.fallback = Some(names.iter().map(|name| (*name).to_owned()).collect());
        self
    }

    pub(crate) fn rewrite_prefix(mut self, replacement: &str) -> Self {
        self.rewrite_prefix = Some(replacement.to_owned());
        self
    }

    pub(crate) fn upstream_pool(mut self, members: &[(&str, u16)], strategy: &'static str) -> Self {
        self.target = Target::Upstream(
            members
                .first()
                .map(|(name, _)| (*name).to_owned())
                .unwrap_or_default(),
        );
        self.upstream_pool = Some((
            members
                .iter()
                .map(|(name, weight)| ((*name).to_owned(), *weight))
                .collect(),
            strategy,
        ));
        self
    }

    pub(crate) fn pool(mut self, members: &[(&str, u16)], strategy: &'static str) -> Self {
        self.target = Target::Provider(
            members
                .first()
                .map(|(name, _)| (*name).to_owned())
                .unwrap_or_default(),
        );
        self.pool = Some((
            members
                .iter()
                .map(|(name, weight)| ((*name).to_owned(), *weight))
                .collect(),
            strategy,
        ));
        self
    }
}
