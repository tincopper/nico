//! Routes: the unit that decides where a request goes.
//!
//! A [`Route`] carries only what routing needs before the request body is read: an optional exact
//! host, a path prefix, and exactly one target. The target is a [`RouteTarget`] rather than two
//! optional fields, so "no target" and "two targets" are unrepresentable once the document is
//! built. Prefix semantics — how `/api/` matches `/api/x` — belong to the data plane, not here.
//!
//! The route also names the plugins that run on its requests. It names them rather than carrying
//! them, so one plugin entry is shared by every route that references it and a route cannot
//! disagree with the plugin it runs.

/// Where a matched route sends the request.
///
/// A data plane must handle the target kinds it knows plus a wildcard: the set is closed for this
/// milestone but a new kind arrives with a new data plane, not with a change to an existing one.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RouteTarget {
    /// Forward to the named [`Upstream`](super::Upstream).
    Upstream(String),
    /// Hand the request to the named [`Provider`](super::Provider).
    Provider(String),
}

impl RouteTarget {
    /// Returns the referenced name, whichever kind of target this is.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Upstream(name) | Self::Provider(name) => name,
        }
    }
}

/// One routing rule.
///
/// Fields are private and there is no public constructor: a `Route` only exists inside a
/// [`Document`](super::Document) that has passed reference and semantic validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub(super) id: String,
    pub(super) host: Option<String>,
    pub(super) path_prefix: String,
    pub(super) target: RouteTarget,
    pub(super) timeout_ms: Option<u32>,
    pub(super) max_body_bytes: Option<u32>,
    pub(super) plugins: Vec<String>,
}

impl Route {
    /// Returns the unique route identifier, used in logs and audit records.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the exact host this route is limited to, when it is not host-independent.
    #[must_use]
    pub fn host(&self) -> Option<&str> {
        self.host.as_deref()
    }

    /// Returns the path prefix this route matches.
    #[must_use]
    pub fn path_prefix(&self) -> &str {
        &self.path_prefix
    }

    /// Returns the upstream or provider this route forwards to.
    #[must_use]
    pub const fn target(&self) -> &RouteTarget {
        &self.target
    }

    /// Returns the route-level timeout in milliseconds, when the document sets one.
    #[must_use]
    pub const fn timeout_ms(&self) -> Option<u32> {
        self.timeout_ms
    }

    /// Returns the route-level request body bound in bytes, when the document sets one.
    #[must_use]
    pub const fn max_body_bytes(&self) -> Option<u32> {
        self.max_body_bytes
    }

    /// Returns the plugins this route runs, in document order.
    ///
    /// The order is the document's, not the execution order: which phase a plugin belongs to is
    /// decided by the crate that implements it, so a filter written before an authentication plugin
    /// does not run before it. An empty list means the route runs no plugins.
    #[must_use]
    pub fn plugins(&self) -> &[String] {
        &self.plugins
    }
}
