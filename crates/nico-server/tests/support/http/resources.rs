//! The resources document the HTTP scenarios serve, rendered from parts.
//!
//! A resources document is a closed schema: `providers`, `upstreams`, `routes` and `plugins` are all
//! required, and a route names exactly one target. Writing one by hand in every scenario would make
//! each test restate rules it does not care about and drift from the schema as it grows, so the
//! builders here render a complete document from the parts a scenario actually varies.
//!
//! This is a submodule of the harness rather than of `support` because it shares the fixture
//! vocabulary — the same route and the same plugin are referenced by every HTTP scenario, and a plugin
//! entry is only meaningful next to a route that names it.

use std::net::SocketAddr;

// --- resources document ------------------------------------------------------

mod plugin;
mod route;
mod upstream;

pub(crate) use self::{plugin::Plugin, route::Route};
use self::{route::Target, upstream::UpstreamEntry};

/// One entry of the document's `providers` collection, rendered from parts.
///
/// A provider entry is an endpoint's configuration, not a policy: the kind is the adapter's registry
/// key, the base URL is where the adapter compiles its endpoint from, and the credential is a
/// reference the publication resolves. The HTTP scenarios all use the OpenAI adapter, so the kind is
/// fixed here; a second adapter would add a constructor rather than a field every scenario spells.
#[derive(Debug, Clone)]
struct ProviderEntry {
    name: String,
    kind: &'static str,
    base_url: String,
    credential_ref: String,
    model_map: Vec<(String, String)>,
    /// Physical model names this provider may serve; empty means no restriction.
    model_allowlist: Vec<String>,
    /// The endpoint's failure budget, written when a scenario tunes it; `None` leaves the document
    /// defaults (threshold 5, cooldown 30s).
    circuit_breaker: Option<(u32, u32)>,
}

/// A complete resources document, rendered from parts.
///
/// The defaults describe a usable document: a 1 MiB body bound, a 30 s whole-request budget, and no
/// providers, upstreams or routes until the test adds one.
#[derive(Debug, Clone)]
pub(crate) struct Resources {
    max_body_bytes: u32,
    max_response_bytes: Option<u32>,
    timeout_ms: u32,
    providers: Vec<ProviderEntry>,
    upstreams: Vec<UpstreamEntry>,
    routes: Vec<Route>,
    plugins: Vec<Plugin>,
    global_plugins: Vec<String>,
}

impl Resources {
    /// A document with the fixture defaults and nothing else.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Sets `request.max_body_bytes`.
    pub(crate) fn body_limit(mut self, bytes: u32) -> Self {
        self.max_body_bytes = bytes;
        self
    }

    /// Sets `request.max_response_bytes`; a route may only tighten this value.
    pub(crate) fn response_limit(mut self, bytes: u32) -> Self {
        self.max_response_bytes = Some(bytes);
        self
    }

    /// Sets `request.timeout_ms`.
    pub(crate) fn timeout_ms(mut self, timeout_ms: u32) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }

    /// Adds the provider a provider route can name.
    ///
    /// `base_url` and `credential_ref` are the scenario's: the publication materializes the reference,
    /// so a fixture that names an `env:` variable nothing sets would be refused — and setting one is
    /// `unsafe` in the 2024 edition and shared with every test in the binary. A scenario therefore
    /// writes a `file:` fixture and points the base URL at a loopback address, which also keeps a test
    /// from depending on a name resolving.
    pub(crate) fn provider(mut self, name: &str, base_url: &str, credential_ref: &str) -> Self {
        self.providers.push(ProviderEntry {
            name: name.to_owned(),
            kind: "openai",
            base_url: base_url.to_owned(),
            credential_ref: credential_ref.to_owned(),
            model_map: Vec::new(),
            model_allowlist: Vec::new(),
            circuit_breaker: None,
        });
        self
    }

    /// Adds the provider a provider route can name, with a logical → physical model map.
    pub(crate) fn provider_with_model_map(
        mut self,
        name: &str,
        base_url: &str,
        credential_ref: &str,
        model_map: &[(&str, &str)],
    ) -> Self {
        self.providers.push(ProviderEntry {
            name: name.to_owned(),
            kind: "openai",
            base_url: base_url.to_owned(),
            credential_ref: credential_ref.to_owned(),
            model_map: model_map
                .iter()
                .map(|(logical, physical)| ((*logical).to_owned(), (*physical).to_owned()))
                .collect(),
            model_allowlist: Vec::new(),
            circuit_breaker: None,
        });
        self
    }

    /// Adds a provider restricted to `allowed` physical model names.
    ///
    /// A pool scenario uses this to make two members serve disjoint model sets, so a request's model
    /// decides which member may be selected before the strategy runs.
    pub(crate) fn provider_with_allowlist(
        mut self,
        name: &str,
        base_url: &str,
        credential_ref: &str,
        allowed: &[&str],
    ) -> Self {
        self.providers.push(ProviderEntry {
            name: name.to_owned(),
            kind: "openai",
            base_url: base_url.to_owned(),
            credential_ref: credential_ref.to_owned(),
            model_map: Vec::new(),
            model_allowlist: allowed.iter().map(|name| (*name).to_owned()).collect(),
            circuit_breaker: None,
        });
        self
    }

    /// Adds a provider of an explicit adapter kind, with a logical → physical model map.
    ///
    /// The scenarios above hard-code the OpenAI kind because that is the adapter they exercise; a
    /// scenario for another dialect names it here, and the publication resolves the kind against the
    /// registry exactly as it would from a written document.
    pub(crate) fn provider_of_kind(
        mut self,
        name: &str,
        kind: &'static str,
        base_url: &str,
        credential_ref: &str,
        model_map: &[(&str, &str)],
    ) -> Self {
        self.providers.push(ProviderEntry {
            name: name.to_owned(),
            kind,
            base_url: base_url.to_owned(),
            credential_ref: credential_ref.to_owned(),
            model_map: model_map
                .iter()
                .map(|(logical, physical)| ((*logical).to_owned(), (*physical).to_owned()))
                .collect(),
            model_allowlist: Vec::new(),
            circuit_breaker: None,
        });
        self
    }

    /// Adds the provider with an explicit circuit-breaker threshold and cooldown, in milliseconds.
    ///
    /// A fallback scenario needs the threshold low so the second target is reached: the document
    /// default of five failures would need five consecutive requests first, and a test drives one.
    /// A `threshold` of `0` disables the breaker, which is the document's own spelling.
    pub(crate) fn provider_with_breaker(
        mut self,
        name: &str,
        base_url: &str,
        credential_ref: &str,
        threshold: u32,
        cooldown_ms: u32,
    ) -> Self {
        self.providers.push(ProviderEntry {
            name: name.to_owned(),
            kind: "openai",
            base_url: base_url.to_owned(),
            credential_ref: credential_ref.to_owned(),
            model_map: Vec::new(),
            model_allowlist: Vec::new(),
            circuit_breaker: Some((threshold, cooldown_ms)),
        });
        self
    }

    /// Adds a plaintext upstream reachable at `address`.
    pub(crate) fn upstream(mut self, name: &str, address: SocketAddr) -> Self {
        self.upstreams.push(UpstreamEntry::plaintext(
            name,
            address.ip().to_string(),
            address.port(),
        ));
        self
    }

    /// Adds an `https` upstream at `host`:`port`.
    ///
    /// `host` is written as given, so a scenario can name the host a certificate is signed for
    /// (`localhost`) or an address it is not (`127.0.0.1`). Resolution happens at request time,
    /// exactly as it does for a written document.
    pub(crate) fn tls_upstream(mut self, name: &str, host: &str, port: u16) -> Self {
        self.upstreams.push(UpstreamEntry::tls(name, host, port));
        self
    }

    /// Adds a plaintext upstream with an explicit member list on one logical host.
    ///
    /// `host` is the identity every member is addressed by; each member is `(name, address)` and
    /// connects to its own address and port. This is the multi-instance form, and the only one whose
    /// requests record an `upstream.endpoint`.
    pub(crate) fn upstream_with_members(
        mut self,
        name: &str,
        host: &str,
        members: &[(&'static str, SocketAddr)],
    ) -> Self {
        self.upstreams
            .push(UpstreamEntry::members(name, "http", host, members));
        self
    }

    /// Adds an `https` upstream with an explicit member list on one logical host.
    ///
    /// The certificate must name `host`, not the members: a fleet of machines is one service.
    pub(crate) fn tls_upstream_with_members(
        mut self,
        name: &str,
        host: &str,
        members: &[(&'static str, SocketAddr)],
    ) -> Self {
        self.upstreams
            .push(UpstreamEntry::members(name, "https", host, members));
        self
    }

    /// Sets the selection rule of the upstream added last.
    ///
    /// A builder step on the collection rather than an argument to every constructor: the strategy is
    /// a property of one entry, and only the scenarios that assert on selection write one.
    pub(crate) fn strategy(mut self, strategy: &'static str) -> Self {
        let upstream = self
            .upstreams
            .pop()
            .expect("a strategy applies to an upstream that was just added");
        self.upstreams.push(upstream.with_strategy(strategy));
        self
    }

    /// Sets the failure budget of the upstream added last.
    ///
    /// `max_fails` of `0` disables ejection, which is the document's own spelling; a scenario that
    /// wants a member to leave rotation after one failure writes `1`.
    pub(crate) fn ejection(mut self, max_fails: u32, fail_timeout_ms: u32) -> Self {
        let upstream = self
            .upstreams
            .pop()
            .expect("a failure budget applies to an upstream that was just added");
        self.upstreams
            .push(upstream.with_ejection(max_fails, fail_timeout_ms));
        self
    }

    /// Adds a route. A provider route is added with [`Resources::provider_route`].
    pub(crate) fn route(mut self, route: Route) -> Self {
        self.routes.push(route);
        self
    }

    /// Adds a plugin entry a route can reference.
    pub(crate) fn plugin(mut self, plugin: Plugin) -> Self {
        self.plugins.push(plugin);
        self
    }

    /// Names the plugins that run on every request, before routing.
    ///
    /// A use site, not a definition: a name listed here must also be declared with
    /// [`Resources::plugin`], and declaring one is not a decision to run it.
    pub(crate) fn global_plugins(mut self, names: &[&str]) -> Self {
        self.global_plugins = names.iter().map(|name| (*name).to_owned()).collect();
        self
    }

    /// Adds a route that forwards to a provider instead of an upstream.
    ///
    /// The route serves `openai.chat` in `convert` mode, which is the document's default for a
    /// provider route; [`Route::protocol`] and [`Route::mode`] vary those facts through
    /// [`Resources::route`].
    pub(crate) fn provider_route(mut self, id: &str, path_prefix: &str, provider: &str) -> Self {
        self.routes.push(Route::provider(id, path_prefix, provider));
        self
    }

    /// Renders the document as YAML.
    pub(crate) fn render(&self) -> String {
        let mut yaml = format!(
            "\
apiVersion: gateway/v1
request:
  max_body_bytes: {body}
",
            body = self.max_body_bytes,
        );
        if let Some(max_response_bytes) = self.max_response_bytes {
            yaml.push_str(&format!("  max_response_bytes: {max_response_bytes}\n"));
        }
        yaml.push_str(&format!(
            "  timeout_ms: {timeout}\n",
            timeout = self.timeout_ms
        ));
        if self.plugins.is_empty() {
            yaml.push_str("plugins: []\n");
        } else {
            yaml.push_str("plugins:\n");
            for plugin in &self.plugins {
                yaml.push_str(&plugin.entry);
            }
        }

        if self.global_plugins.is_empty() {
            yaml.push_str("global_plugins: []\n");
        } else {
            yaml.push_str(&format!(
                "global_plugins: [{}]\n",
                self.global_plugins.join(", ")
            ));
        }

        if self.providers.is_empty() {
            yaml.push_str("providers: []\n");
        } else {
            yaml.push_str("providers:\n");
            for provider in &self.providers {
                yaml.push_str(&format!(
                    "  - name: {name}\n    kind: {kind}\n    base_url: {base_url}\n    credential_ref: {credential_ref}\n",
                    name = provider.name,
                    kind = provider.kind,
                    base_url = provider.base_url,
                    credential_ref = provider.credential_ref,
                ));
                if !provider.model_map.is_empty() {
                    yaml.push_str("    model_map:\n");
                    for (logical, physical) in &provider.model_map {
                        yaml.push_str(&format!("      {logical}: {physical}\n"));
                    }
                }
                if !provider.model_allowlist.is_empty() {
                    yaml.push_str(&format!(
                        "    model_allowlist: [{}]\n",
                        provider.model_allowlist.join(", ")
                    ));
                }
                if let Some((threshold, cooldown_ms)) = provider.circuit_breaker {
                    yaml.push_str("    circuit_breaker:\n");
                    yaml.push_str(&format!("      failure_threshold: {threshold}\n"));
                    yaml.push_str(&format!("      cooldown_ms: {cooldown_ms}\n"));
                }
            }
        }

        if self.upstreams.is_empty() {
            yaml.push_str("upstreams: []\n");
        } else {
            yaml.push_str("upstreams:\n");
            for upstream in &self.upstreams {
                yaml.push_str(&upstream.render());
            }
        }

        if self.routes.is_empty() {
            yaml.push_str("routes: []\n");
        } else {
            yaml.push_str("routes:\n");
            for route in &self.routes {
                yaml.push_str(&format!(
                    "  - id: {id}\n    path_prefix: {prefix}\n",
                    id = route.id,
                    prefix = route.path_prefix,
                ));
                if let Some(host) = &route.host {
                    yaml.push_str(&format!("    host: {host}\n"));
                }
                if let Some(replacement) = &route.rewrite_prefix {
                    yaml.push_str(&format!("    rewrite_prefix: {replacement}\n"));
                }
                match &route.target {
                    // A pooled upstream route writes its pool below instead of the single target:
                    // the document refuses both at once, so the two arms are exclusive by
                    // construction — the same shape a provider pool renders.
                    Target::Upstream(_) if route.upstream_pool.is_some() => {}
                    Target::Upstream(name) => yaml.push_str(&format!("    upstream: {name}\n")),
                    // The document model requires a client protocol on a provider route, and `mode`
                    // and `max_response_bytes` exist on a provider route alone. The builder keeps
                    // the protocol on the provider target so an upstream route cannot name one by
                    // accident; `mode` and the response bound are written below when set.
                    // A pool route writes `providers` instead of `provider`, and the document
                    // refuses both at once — so the two arms are exclusive by construction. The
                    // protocol is still the route's own: a pool distributes across providers, it does
                    // not change what the caller speaks.
                    Target::Provider(_) if route.pool.is_some() => {
                        let protocol = route.protocol.as_deref().unwrap_or("openai.chat");
                        yaml.push_str(&format!("    protocol: {protocol}\n"));
                    }
                    Target::Provider(name) => {
                        let protocol = route.protocol.as_deref().unwrap_or("openai.chat");
                        yaml.push_str(&format!("    provider: {name}\n    protocol: {protocol}\n"));
                    }
                }
                if let Some((members, strategy)) = &route.pool {
                    yaml.push_str(&format!("    strategy: {strategy}\n"));
                    yaml.push_str("    providers:\n");
                    for (name, weight) in members {
                        yaml.push_str(&format!("      - name: {name}\n"));
                        if *weight != 1 {
                            yaml.push_str(&format!("        weight: {weight}\n"));
                        }
                    }
                }
                if let Some((members, strategy)) = &route.upstream_pool {
                    yaml.push_str(&format!("    strategy: {strategy}\n"));
                    yaml.push_str("    upstreams:\n");
                    for (name, weight) in members {
                        yaml.push_str(&format!("      - upstream: {name}\n"));
                        if *weight != 1 {
                            yaml.push_str(&format!("        weight: {weight}\n"));
                        }
                    }
                }
                if let Some(mode) = &route.mode {
                    yaml.push_str(&format!("    mode: {mode}\n"));
                }
                if let Some(max_response_bytes) = route.max_response_bytes {
                    yaml.push_str(&format!("    max_response_bytes: {max_response_bytes}\n"));
                }
                if let Some(timeout_ms) = route.timeout_ms {
                    yaml.push_str(&format!("    timeout_ms: {timeout_ms}\n"));
                }
                if let Some(max_body_bytes) = route.max_body_bytes {
                    yaml.push_str(&format!("    max_body_bytes: {max_body_bytes}\n"));
                }
                if !route.plugins.is_empty() {
                    yaml.push_str("    plugins:\n");
                    for name in &route.plugins {
                        yaml.push_str(&format!("      - {name}\n"));
                    }
                }
                if let Some(ttft_timeout_ms) = route.ttft_timeout_ms {
                    yaml.push_str(&format!("    ttft_timeout_ms: {ttft_timeout_ms}\n"));
                }
                if let Some(retry_budget) = route.retry_budget {
                    yaml.push_str(&format!("    retry_budget: {retry_budget}\n"));
                }
                if let Some(fallback) = &route.fallback {
                    yaml.push_str(&format!("    fallback: [{}]\n", fallback.join(", ")));
                }
            }
        }

        yaml
    }
}

impl Default for Resources {
    fn default() -> Self {
        Self {
            max_body_bytes: 1024 * 1024,
            max_response_bytes: None,
            timeout_ms: 30_000,
            providers: Vec::new(),
            upstreams: Vec::new(),
            routes: Vec::new(),
            plugins: Vec::new(),
            global_plugins: Vec::new(),
        }
    }
}
