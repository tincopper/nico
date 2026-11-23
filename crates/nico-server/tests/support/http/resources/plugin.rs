//! One `plugins` entry of a fixture document.
//!
//! A plugin entry is an envelope (name, kind, enabled, `buffers_body`, the refusal presentation) plus a
//! `config` block whose fields belong to the kind. The builders here state the entries a scenario
//! needs, one constructor per kind, and leave every field the scenario does not vary to the schema's
//! defaults — a test that had to spell all of them would hide which one it meant to exercise.
//!
//! The `mock` kind exists only in this harness: it lets a scenario observe the order the scopes ran in
//! without asserting anything about a real policy's refusals.

/// One entry of the document's `plugins` collection, rendered from parts.
///
/// A plugin's fields are the kind's own, so the builders here state the ones a scenario varies and
/// leave the rest to the schema's defaults: a test that had to spell every field would hide which
/// one it meant to exercise.
#[derive(Debug, Clone)]
pub(crate) struct Plugin {
    /// The `name`/`kind` lines plus any envelope field and the `config:` block, in the order written.
    pub(super) entry: String,
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

    /// A `wasm` entry: the artifact path the document names, plus the kind's config fields.
    pub(crate) fn wasm(name: &str, module_ref: &str, config_fields: &str) -> Self {
        Self::new(name, "wasm").fields(&format!("      module: {module_ref}\n{config_fields}"))
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
