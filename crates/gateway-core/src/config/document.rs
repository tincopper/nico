//! The validated document model: the document envelope and the collections it carries.
//!
//! [`Document::parse`] is the only constructor. Parsing and its resource bounds live in
//! [`super::parse`], assembling the validated collections lives in [`super::validate`], and this
//! module owns the [`Document`] the rest of the process reads. Because fields are private and no
//! public constructor exists, every `Document` has passed all validation stages.

use super::{
    bounds::ParseLimits, error::DocumentError, limits::Limits, parse, provider::Provider,
    route::Route, upstream::Upstream,
};

/// The only `apiVersion` this build understands.
pub const API_VERSION: &str = "gateway/v1";

/// A validated gateway resources document.
///
/// Fields stay private and the type implements neither [`serde::Deserialize`] nor a public
/// constructor: a `Document` can only be built by [`Document::parse`], so every instance has passed
/// size, encoding, shape, reference and semantic validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    api_version: String,
    limits: Limits,
    providers: Vec<Provider>,
    upstreams: Vec<Upstream>,
    routes: Vec<Route>,
}

impl Document {
    /// Parses and validates a complete resources document.
    ///
    /// Validation order is fixed: size bound, UTF-8, YAML anchor/alias rejection, YAML syntax, node
    /// and depth bounds, schema (unknown fields and missing sections are rejected), `apiVersion`,
    /// limit ranges, reference and target validation, semantic validation, then count bounds.
    ///
    /// # Errors
    /// - [`DocumentError::TooLarge`] when `bytes` is longer than `limits.max_bytes`.
    /// - [`DocumentError::NotUtf8`] when `bytes` is not valid UTF-8.
    /// - [`DocumentError::AliasForbidden`] when the document uses a YAML anchor or alias.
    /// - [`DocumentError::Yaml`] when the document is not YAML or violates the schema.
    /// - [`DocumentError::TooManyNodes`] / [`DocumentError::TooDeep`] when the document exceeds the
    ///   shape bounds in [`ParseLimits::schema`].
    /// - [`DocumentError::WrongApiVersion`] when `apiVersion` is not [`API_VERSION`].
    /// - [`DocumentError::LimitOutOfRange`] when a limit is outside its accepted range.
    /// - [`DocumentError::DuplicateId`] when a route id, an upstream name, a provider name or a
    ///   `(host, path_prefix)` match combination repeats.
    /// - [`DocumentError::RouteTargetMissing`] / [`DocumentError::RouteTargetAmbiguous`] when a
    ///   route does not name exactly one upstream or provider.
    /// - [`DocumentError::ReferenceUnknown`] when a route names an upstream or provider that is not
    ///   defined.
    /// - [`DocumentError::InvalidField`] when a name, host, path prefix, base URL, credential
    ///   reference, upstream host or port is malformed.
    /// - [`DocumentError::TooMany`] when a collection exceeds its count bound.
    ///
    /// # Examples
    ///
    /// ```
    /// use gateway_core::{Document, ParseLimits, RouteTarget};
    ///
    /// let bytes = "\
    /// apiVersion: gateway/v1
    /// limits:
    ///   max_request_body_bytes: 1048576
    ///   request_timeout_ms: 30000
    /// providers:
    ///   - name: openai-primary
    ///     kind: openai
    ///     base_url: https://api.openai.com
    ///     credential_ref: env:OPENAI_API_KEY
    /// upstreams:
    ///   - name: api-backend
    ///     scheme: http
    ///     host: 127.0.0.1
    ///     port: 8080
    /// routes:
    ///   - id: api-public-v1
    ///     path_prefix: /api/
    ///     upstream: api-backend
    ///   - id: ai-chat
    ///     path_prefix: /v1/chat/
    ///     provider: openai-primary
    /// ";
    /// let document = Document::parse(bytes.as_bytes(), ParseLimits::new(1024 * 1024))
    ///     .expect("fixture is valid");
    /// assert_eq!(document.limits().request_timeout_ms(), 30_000);
    /// assert_eq!(document.routes().len(), 2);
    /// assert_eq!(
    ///     document.routes()[1].target(),
    ///     &RouteTarget::Provider("openai-primary".to_owned())
    /// );
    /// ```
    ///
    /// A route that names an upstream the document never defines is rejected, not forwarded:
    ///
    /// ```
    /// use gateway_core::{Document, DocumentError, ErrorCategory, ParseLimits};
    ///
    /// let bytes = "\
    /// apiVersion: gateway/v1
    /// limits:
    ///   max_request_body_bytes: 1024
    ///   request_timeout_ms: 1000
    /// providers: []
    /// upstreams: []
    /// routes:
    ///   - id: api-public-v1
    ///     path_prefix: /api/
    ///     upstream: api-backend
    /// ";
    /// let error = Document::parse(bytes.as_bytes(), ParseLimits::new(1024))
    ///     .expect_err("no upstream is defined");
    /// assert!(matches!(
    ///     error,
    ///     DocumentError::ReferenceUnknown {
    ///         kind: "upstream",
    ///         ..
    ///     }
    /// ));
    /// assert_eq!(error.category(), ErrorCategory::Client);
    /// ```
    pub fn parse(bytes: &[u8], limits: ParseLimits) -> Result<Self, DocumentError> {
        parse::parse(bytes, limits)
    }

    /// Builds a document from parts that already passed every validation stage.
    pub(super) const fn new(
        api_version: String,
        limits: Limits,
        providers: Vec<Provider>,
        upstreams: Vec<Upstream>,
        routes: Vec<Route>,
    ) -> Self {
        Self {
            api_version,
            limits,
            providers,
            upstreams,
            routes,
        }
    }

    /// Returns the declared `apiVersion`, always [`API_VERSION`] for a parsed document.
    #[must_use]
    pub fn api_version(&self) -> &str {
        &self.api_version
    }

    /// Returns the validated global limits.
    #[must_use]
    pub const fn limits(&self) -> Limits {
        self.limits
    }

    /// Returns the configured providers, in document order.
    #[must_use]
    pub fn providers(&self) -> &[Provider] {
        &self.providers
    }

    /// Returns the configured upstreams, in document order.
    #[must_use]
    pub fn upstreams(&self) -> &[Upstream] {
        &self.upstreams
    }

    /// Returns the configured routes, in document order.
    ///
    /// Document order is not match order: precedence is defined by the data plane.
    #[must_use]
    pub fn routes(&self) -> &[Route] {
        &self.routes
    }
}

#[cfg(test)]
mod tests;
