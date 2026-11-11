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

mod route;
mod upstream;

pub(crate) use self::route::Route;
use self::{route::Target, upstream::UpstreamEntry};

/// One entry of the document's `plugins` collection, rendered from parts.
///
/// A plugin's fields are the kind's own, so the builders here state the ones a scenario varies and
/// leave the rest to the schema's defaults: a test that had to spell every field would hide which
/// one it meant to exercise.
#[derive(Debug, Clone)]
pub(crate) struct Plugin {
    /// The `name`/`kind` lines plus any envelope field and the `config:` block, in the order written.
    entry: String,
}

impl Plugin {
    /// A `jwt` entry: HS256, one key named `k1`, signed by the credential `secret_ref` names.
    pub(crate) fn jwt(name: &str, secret_ref: &str) -> Self {
        Self::new(name, "jwt").fields(&format!(
            "      keys:\n        - kid: k1\n          secret_ref: {secret_ref}\n"
        ))
    }

    /// An `api_key` entry: one accepted key, named `partner`, in the default `authorization` header.
    pub(crate) fn api_key(name: &str, secret_ref: &str) -> Self {
        Self::new(name, "api_key").fields(&format!(
            "      keys:\n        - name: partner\n          secret_ref: {secret_ref}\n"
        ))
    }

    /// An `api_key` entry that reads its key from `header` instead of `authorization`.
    pub(crate) fn api_key_in(name: &str, header: &str, secret_ref: &str) -> Self {
        Self::new(name, "api_key").fields(&format!(
            "      header: {header}\n      keys:\n        - name: partner\n          secret_ref: {secret_ref}\n"
        ))
    }

    /// A `rate_limit` entry: one bucket per caller, keyed by whatever identity exists.
    pub(crate) fn rate_limit(name: &str, burst: u32, per_second: u32) -> Self {
        Self::new(name, "rate_limit").fields(&format!(
            "      burst: {burst}\n      per_second: {per_second}\n"
        ))
    }

    /// A `mock` entry that appends `value` to the `header` request header.
    ///
    /// The kind exists only in the test harness, so a scenario can observe the order the scopes ran
    /// in without asserting anything about a real policy's refusals.
    pub(crate) fn mock_tag(name: &str, header: &str, value: &str) -> Self {
        Self::mock(
            name,
            &format!("      action: tag\n      header: {header}\n      value: {value}\n"),
        )
    }

    /// A `mock` entry that answers the request itself with `status`, `headers` and `body`.
    pub(crate) fn mock_respond(
        name: &str,
        status: u16,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Self {
        let mut config = format!("      action: respond\n      status: {status}\n");
        if !headers.is_empty() {
            config.push_str("      headers:\n");
            for (header, value) in headers {
                config.push_str(&format!("        - [{header}, \"{value}\"]\n"));
            }
        }
        config.push_str(&format!("      body: \"{body}\"\n"));
        Self::mock(name, &config)
    }

    /// A `mock` entry that rewrites the whole request body as `prefix + body + suffix`.
    ///
    /// Only meaningful on a route whose chain declares `buffers_body`; the entry that declares it is
    /// the one built with [`Plugin::buffers_body`].
    pub(crate) fn mock_rewrite_body(name: &str, prefix: &str, suffix: &str) -> Self {
        Self::mock(
            name,
            &format!(
                "      action: rewrite_body\n      prefix: \"{prefix}\"\n      suffix: \"{suffix}\"\n"
            ),
        )
    }

    /// A `mock` entry that appends `suffix` to every response-body chunk.
    pub(crate) fn mock_append_response(name: &str, suffix: &str) -> Self {
        Self::mock(
            name,
            &format!("      action: append_response\n      suffix: \"{suffix}\"\n"),
        )
    }

    /// Writes the `name` and `kind` lines of an entry.
    fn new(name: &str, kind: &str) -> Self {
        Self {
            entry: format!("  - name: {name}\n    kind: {kind}\n"),
        }
    }

    /// Writes a `mock` entry with `config` as the block below `config:`.
    fn mock(name: &str, config: &str) -> Self {
        Self::new(name, "mock").fields(config)
    }

    /// Appends the kind's own configuration below a `config:` line.
    fn fields(mut self, fields: &str) -> Self {
        self.entry.push_str("    config:\n");
        self.entry.push_str(fields);
        self
    }

    /// Declares that this instance needs the whole request body in one call.
    ///
    /// A field of the envelope, written at the same level as `kind` and `config` rather than under
    /// `config`: it decides how the data plane delivers the body, not what the kind does with it.
    pub(crate) fn buffers_body(mut self) -> Self {
        self.entry.push_str("    buffers_body: true\n");
        self
    }

    /// Configures the refusal this instance presents.
    ///
    /// `body` is the YAML value the entry writes under `body:`, so a JSON body is a flow mapping
    /// (`{detail: \"${message}\"}`) and any other content type is a quoted string. The shape follows the
    /// content type because the schema decides it that way, and a test that wanted to spell one shape
    /// under the other could not.
    pub(crate) fn error_response(mut self, status: u16, content_type: &str, body: &str) -> Self {
        self.entry.push_str(&format!(
            "    error_response:\n      status: {status}\n      content_type: {content_type}\n      body: {body}\n"
        ));
        self
    }
}

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
                match &route.target {
                    Target::Upstream(name) => yaml.push_str(&format!("    upstream: {name}\n")),
                    // The document model requires a client protocol on a provider route, and `mode`
                    // and `max_response_bytes` exist on a provider route alone. The builder keeps
                    // the protocol on the provider target so an upstream route cannot name one by
                    // accident; `mode` and the response bound are written below when set.
                    Target::Provider(name) => {
                        let protocol = route.protocol.as_deref().unwrap_or("openai.chat");
                        yaml.push_str(&format!("    provider: {name}\n    protocol: {protocol}\n"));
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
