//! The `plugins` collection: one **envelope** per named policy a route references.
//!
//! An entry says who it is (`name`), which implementation runs it (`kind`), whether it is live
//! (`enabled`), whether it buffers the request body, how it refuses a caller (`error_response`), and
//! carries the kind's own configuration as an opaque value under `config`.
//!
//! This module owns the **envelope and only the envelope**. `kind` is a string, not an enum: which
//! kinds exist is decided by a registry in the crate that compiles plugins, so adding one changes no
//! document model and this crate keeps holding no policy vocabulary at all. What a kind accepts under
//! `config` — its field names, ranges, defaults and credential references — is owned by that kind's
//! implementation and validated while the document is compiled, which is still the publication path
//! and still keeps the last known good document in service when it fails.
//!
//! What stays here is what every plugin shares: the name a route references, the enabled switch, the
//! request-body mode, and a rejection presentation modelled as data rather than as a template
//! language. [`ErrorBody`] separates a structured JSON value from a literal string, and the body shape
//! is read from the entry's own effective content type. Parsing a template's variables is the
//! implementing crate's job — this module only guarantees that whatever it parses came from a shape
//! the document allows.

use super::error::DocumentError;

mod rejection;

pub use rejection::{BodyValue, ErrorBody, ErrorResponse};

/// Content type a rejection body uses when the entry does not configure one.
///
/// The built-in presentation is a JSON envelope, so JSON is also the shape a body is read as when no
/// content type is written: one default, in one place. It is exported because the crate that
/// implements a plugin must also know which content types describe a JSON body — see
/// [`is_json_content_type`].
pub const DEFAULT_CONTENT_TYPE: &str = "application/json";

/// Reports whether `content_type` names JSON, with or without parameters.
///
/// The document model decides a rejection body's shape from this predicate, and so does the layer
/// that compiles the body: "`application/json` means a structured value" is one rule, and a second
/// copy of it would eventually disagree with this one about `; charset=utf-8`.
#[must_use]
pub fn is_json_content_type(content_type: &str) -> bool {
    content_type.trim() == DEFAULT_CONTENT_TYPE
        || content_type
            .trim()
            .strip_prefix(DEFAULT_CONTENT_TYPE)
            .is_some_and(|rest| rest.starts_with(';'))
}

/// One entry of the `plugins` collection.
///
/// Fields are private and there is no public constructor: a `PluginSpec` only exists inside a
/// [`Document`](super::Document) that has passed reference and semantic validation, which is what
/// makes "a route references a plugin that does not exist" unrepresentable afterwards.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginSpec {
    pub(super) name: String,
    pub(super) kind: String,
    pub(super) enabled: bool,
    pub(super) buffers_body: bool,
    pub(super) error_response: Option<ErrorResponse>,
    pub(super) config: serde_norway::Value,
}

impl PluginSpec {
    /// Returns the plugin name routes reference.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the kind the entry names, exactly as the document spelled it.
    ///
    /// The value is an opaque string here: the document model does not know which kinds exist, so an
    /// unknown kind is not a document error. The registry of the crate that compiles plugins answers
    /// that question, and an entry whose kind no implementation answers to is refused at publication
    /// rather than skipped — a skipped authentication plugin is an open route.
    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// Returns whether the entry is live.
    ///
    /// A disabled entry is still a declared plugin, so a route may keep referencing it; it is simply
    /// not compiled and never runs. That is the point: taking a policy out of service must not
    /// require editing every route that names it.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Returns whether the kind asked for the whole request body in one call.
    ///
    /// This is a request-side mode only: a response may be an unbounded stream, so it is always
    /// delivered in chunks and a plugin cannot ask for more.
    #[must_use]
    pub const fn buffers_body(&self) -> bool {
        self.buffers_body
    }

    /// Returns this plugin's rejection presentation, when the entry configures one.
    ///
    /// A refusal is part of every plugin that refuses, so the presentation sits on the envelope: a
    /// kind's own configuration never carries it, and changing how a document refuses a caller does
    /// not change what a kind accepts.
    #[must_use]
    pub const fn error_response(&self) -> Option<&ErrorResponse> {
        self.error_response.as_ref()
    }

    /// Returns the kind's own configuration, exactly as the document wrote it.
    ///
    /// The document model does not interpret this value: its structure is owned by the
    /// implementation that answers to [`PluginSpec::kind`], which deserializes it into its own
    /// strongly typed configuration (with `deny_unknown_fields`) while the document is compiled. An
    /// entry that writes no `config` returns [`serde_norway::Value::Null`].
    #[must_use]
    pub const fn config(&self) -> &serde_norway::Value {
        &self.config
    }
}

/// Builds one [`PluginSpec`] from its raw shape.
///
/// Two things happen here, and nothing else: the envelope's own invariant — a kind that names an
/// implementation — is checked, and a rejection body is converted to its JSON shape, because whether
/// it must be structured or a literal string is decided by the entry's own content type. Everything
/// below the envelope is copied through untouched, so this function never decides policy.
///
/// # Errors
/// - [`DocumentError::InvalidField`] when `kind` is empty: an entry that names no implementation can
///   never be compiled, and reporting it here names the field rather than the plugin.
/// - [`DocumentError::InvalidField`] when a rejection body cannot be expressed as the shape its
///   content type requires — a YAML tag, a non-string object key, a non-finite number, or a literal
///   body for a JSON content type (and the reverse).
pub(super) fn spec_from_raw(raw: super::raw::RawPlugin) -> Result<PluginSpec, DocumentError> {
    let super::raw::RawPlugin {
        name,
        kind,
        enabled,
        buffers_body,
        error_response,
        config,
    } = raw;

    if kind.trim().is_empty() {
        return Err(DocumentError::InvalidField {
            field: "plugins[].kind",
            reason: "must not be empty".into(),
        });
    }

    Ok(PluginSpec {
        name,
        kind,
        enabled,
        buffers_body,
        error_response: error_response
            .map(rejection::build_error_response)
            .transpose()?,
        config,
    })
}

#[cfg(test)]
mod tests;
