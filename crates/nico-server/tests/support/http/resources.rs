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

/// What a route forwards to: exactly one of the two, like the document itself.
#[derive(Debug, Clone)]
enum Target {
    Upstream(String),
    Provider(String),
}

/// One `routes` entry of a fixture document.
#[derive(Debug, Clone)]
pub(crate) struct Route {
    id: String,
    host: Option<String>,
    path_prefix: String,
    target: Target,
    timeout_ms: Option<u32>,
    max_body_bytes: Option<u32>,
    plugins: Vec<String>,
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
            plugins: Vec::new(),
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

    /// Names the plugins the route runs, in the order the document lists them.
    pub(crate) fn plugins(mut self, names: &[&str]) -> Self {
        self.plugins = names.iter().map(|name| (*name).to_owned()).collect();
        self
    }
}

/// One entry of the document's `plugins` collection, rendered from parts.
///
/// A plugin's fields are the kind's own, so the builders here state the ones a scenario varies and
/// leave the rest to the schema's defaults: a test that had to spell every field would hide which
/// one it meant to exercise.
#[derive(Debug, Clone)]
pub(crate) struct Plugin {
    entry: String,
}

impl Plugin {
    /// A `jwt` entry: HS256, one key named `k1`, signed by the credential `secret_ref` names.
    pub(crate) fn jwt(name: &str, secret_ref: &str) -> Self {
        Self::new(name, "jwt").fields(&format!(
            "      keys:\n        - kid: k1\n          secret_ref: {secret_ref}\n"
        ))
    }

    /// An `api_key` entry: one accepted key, named `partner`.
    pub(crate) fn api_key(name: &str, secret_ref: &str) -> Self {
        Self::new(name, "api_key").fields(&format!(
            "      keys:\n        - name: partner\n          secret_ref: {secret_ref}\n"
        ))
    }

    /// A `rate_limit` entry: one bucket per caller, keyed by whatever identity exists.
    pub(crate) fn rate_limit(name: &str, burst: u32, per_second: u32) -> Self {
        Self::new(name, "rate_limit").fields(&format!(
            "      burst: {burst}\n      per_second: {per_second}\n"
        ))
    }

    /// Writes the `name`, `kind` and `config:` lines of an entry.
    fn new(name: &str, kind: &str) -> Self {
        Self {
            entry: format!("  - name: {name}\n    kind: {kind}\n    config:\n"),
        }
    }

    /// Appends the kind's own configuration, already indented below `config:`.
    fn fields(mut self, fields: &str) -> Self {
        self.entry.push_str(fields);
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

/// A complete resources document, rendered from parts.
///
/// The defaults describe a usable document: a 1 MiB body bound, a 30 s whole-request budget, and no
/// providers, upstreams or routes until the test adds one.
#[derive(Debug, Clone)]
pub(crate) struct Resources {
    max_body_bytes: u32,
    timeout_ms: u32,
    providers: Vec<String>,
    upstreams: Vec<(String, SocketAddr)>,
    routes: Vec<Route>,
    plugins: Vec<Plugin>,
}

impl Resources {
    /// A document with the fixture defaults and nothing else.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Sets `limits.max_body_bytes`.
    pub(crate) fn body_limit(mut self, bytes: u32) -> Self {
        self.max_body_bytes = bytes;
        self
    }

    /// Sets `limits.timeout_ms`.
    pub(crate) fn timeout_ms(mut self, timeout_ms: u32) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }

    /// Adds the provider a provider route can name.
    pub(crate) fn provider(mut self, name: &str) -> Self {
        self.providers.push(name.to_owned());
        self
    }

    /// Adds an upstream reachable at `address`.
    pub(crate) fn upstream(mut self, name: &str, address: SocketAddr) -> Self {
        self.upstreams.push((name.to_owned(), address));
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

    /// Adds a route that forwards to a provider instead of an upstream.
    pub(crate) fn provider_route(mut self, id: &str, path_prefix: &str, provider: &str) -> Self {
        let route = Route {
            id: id.to_owned(),
            host: None,
            path_prefix: path_prefix.to_owned(),
            target: Target::Provider(provider.to_owned()),
            timeout_ms: None,
            max_body_bytes: None,
            plugins: Vec::new(),
        };
        self.routes.push(route);
        self
    }

    /// Renders the document as YAML.
    pub(crate) fn render(&self) -> String {
        let mut yaml = format!(
            "\
apiVersion: gateway/v1
request:
  max_body_bytes: {body}
  timeout_ms: {timeout}
",
            body = self.max_body_bytes,
            timeout = self.timeout_ms,
        );
        if self.plugins.is_empty() {
            yaml.push_str("plugins: []\n");
        } else {
            yaml.push_str("plugins:\n");
            for plugin in &self.plugins {
                yaml.push_str(&plugin.entry);
            }
        }

        if self.providers.is_empty() {
            yaml.push_str("providers: []\n");
        } else {
            yaml.push_str("providers:\n");
            for name in &self.providers {
                yaml.push_str(&format!(
                    "  - name: {name}\n    kind: openai\n    base_url: https://api.example.test\n    credential_ref: env:FIXTURE_KEY\n"
                ));
            }
        }

        if self.upstreams.is_empty() {
            yaml.push_str("upstreams: []\n");
        } else {
            yaml.push_str("upstreams:\n");
            for (name, address) in &self.upstreams {
                yaml.push_str(&format!(
                    "  - name: {name}\n    scheme: http\n    host: {host}\n    port: {port}\n",
                    host = address.ip(),
                    port = address.port(),
                ));
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
                    Target::Provider(name) => yaml.push_str(&format!("    provider: {name}\n")),
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
            }
        }

        yaml
    }
}

impl Default for Resources {
    fn default() -> Self {
        Self {
            max_body_bytes: 1024 * 1024,
            timeout_ms: 30_000,
            providers: Vec::new(),
            upstreams: Vec::new(),
            routes: Vec::new(),
            plugins: Vec::new(),
        }
    }
}
