//! The identity of the caller, as established by an authenticating plugin.
//!
//! One typed slot rather than a request header: a header can be forged by the client and read by every
//! other plugin, while this slot can only be written by a plugin that authenticated the request and
//! cannot carry anything the caller sent.

use std::sync::Arc;

/// The subject an authenticating plugin established, its optional tenant and the instance that established it.
///
/// `source` names the plugin **instance**, which makes a subject attributable in logs without
/// `nico-http` learning any policy vocabulary. `tenant` is a separate quota grouping fact: a virtual
/// key keeps its own subject while sharing a tenant's budget.
///
/// `source` is an `Arc<str>` rather than the `&str` [`Plugin::name`](crate::Plugin::name) returns,
/// because it is the name a document gave the entry, and the same route may run two entries of one
/// kind: each has to key its own rate-limit bucket and appear under its own `authn` value. Sharing one
/// allocation across requests keeps the per-request cost to a refcount bump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    subject: String,
    tenant: Option<String>,
    source: Arc<str>,
}

impl Identity {
    /// Records the subject `source` authenticated, without a tenant dimension.
    #[must_use]
    pub fn new(subject: impl Into<String>, source: impl Into<Arc<str>>) -> Self {
        Self::with_tenant(subject, None, source)
    }

    /// Records the subject, optional tenant quota dimension, and authenticating plugin instance.
    #[must_use]
    pub fn with_tenant(
        subject: impl Into<String>,
        tenant: Option<String>,
        source: impl Into<Arc<str>>,
    ) -> Self {
        Self {
            subject: subject.into(),
            tenant,
            source: source.into(),
        }
    }

    /// Returns the subject the authenticating plugin established.
    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }

    /// Returns the optional tenant quota grouping identifier.
    #[must_use]
    pub fn tenant(&self) -> Option<&str> {
        self.tenant.as_deref()
    }

    /// Returns the plugin instance that claimed this request.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }
}
