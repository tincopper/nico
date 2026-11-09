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
//!
//! A provider target additionally decides how the exchange is translated: [`Route::protocol`] names
//! the client protocol, [`Route::mode`] whether the body is converted or passed through, and
//! [`Route::max_response_bytes`] how much of a buffered response is accepted. Those three describe a
//! converted exchange, so they exist on a provider route alone; an upstream route forwards bytes and
//! the document refuses them there instead of ignoring them. The same holds for the attempt fields —
//! [`Route::retry_budget`] bounds the tries one target absorbs and [`Route::fallback`] names the
//! providers tried after it — because both decide how a *converted* exchange may fail, which an
//! upstream route has no way to observe.

use serde::Deserialize;

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

/// How a provider route relates the client's body to the provider's.
///
/// The mode is a property of the route, not of the protocol pair: the same two protocols may be
/// byte-compatible for one route and not for another (a caller sending an extension the canonical
/// model does not carry, for example), so the document decides per route. `Convert` is the default
/// because it is the mode that works between two different protocols at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum RouteMode {
    /// Decode the client body into the canonical model, then encode it for the provider.
    #[default]
    Convert,
    /// Forward the client's bytes to the provider unchanged, skipping the canonical model.
    Passthrough,
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
    pub(super) protocol: Option<String>,
    pub(super) mode: RouteMode,
    pub(super) max_response_bytes: Option<u32>,
    pub(super) plugins: Vec<String>,
    pub(super) retry_budget: u8,
    pub(super) fallback: Vec<String>,
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

    /// Returns the client protocol a provider route converts, or `None` for an upstream route.
    ///
    /// The value is a name such as `<vendor>.<surface>`, kept exactly as written. Whether a codec is
    /// registered for it is decided when the document is published, by the crate that owns the
    /// registry: this model only checks that the value could name one.
    #[must_use]
    pub fn protocol(&self) -> Option<&str> {
        self.protocol.as_deref()
    }

    /// Returns how this route relates the client's body to the provider's.
    ///
    /// A provider route defaults to [`RouteMode::Convert`]. An upstream route has no conversion and
    /// the document refuses a `mode` there, so this reports the default for it.
    #[must_use]
    pub const fn mode(&self) -> RouteMode {
        self.mode
    }

    /// Returns the route-level ceiling on a buffered response body, when the document sets one.
    ///
    /// A route may only tighten the document-wide bound, and `None` means it inherits it. Only a
    /// provider route has a response to convert, so the document refuses this on an upstream route.
    #[must_use]
    pub const fn max_response_bytes(&self) -> Option<u32> {
        self.max_response_bytes
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

    /// Returns the total attempts this route may make against one provider target, including the
    /// first.
    ///
    /// The document's default is `1`, which is the behavior of a route that never names the field:
    /// one attempt, no retry. The value bounds the retries a failing target absorbs; moving to
    /// another target is the fallback chain's decision, not this budget's.
    #[must_use]
    pub const fn retry_budget(&self) -> u8 {
        self.retry_budget
    }

    /// Returns the provider names this route tries in order when its target fails, in document
    /// order.
    ///
    /// The names reference [`Provider`](super::Provider) entries, resolved while the document is
    /// validated; whether a member can actually be served is the publication's decision. An empty
    /// list means the route has no fallback, and a `passthrough` route is refused one at
    /// publication because it has no failure classification to retry on.
    #[must_use]
    pub fn fallback(&self) -> &[String] {
        &self.fallback
    }
}
