//! One passthrough route: the endpoint, and nothing that would decode a body.
//!
//! A passthrough route is the escape hatch for a client protocol that already *is* the provider's.
//! The data plane forwards both bodies byte for byte, so the published value only has to answer where
//! the request goes; the exchange, the canonical model and the capability gate never run.
//!
//! # Construction is the whole validation
//!
//! Building one can fail in four ways, and each is a **publication** failure rather than a request
//! one:
//!
//! - the route converts, so it has no passthrough front-end;
//! - the adapter serves a kind other than the route's;
//! - the adapter no longer speaks the protocol the route passes through — the client's bytes would be
//!   forwarded to a peer that parses something else, which is exactly the silent corruption the
//!   mode exists to avoid;
//! - the route's base URL is not addressable.
//!
//! Checking them here rather than per request means a published passthrough route is legal by
//! construction, and every check happens while the document is compiled, where a refusal keeps the
//! last known good generation serving.

use std::{fmt, sync::Arc};

use nico_http::{
    Destination,
    llm::{ConvertingRoute, LlmRoute},
};

use crate::provider::{Endpoint, ExchangeMode, Provider, ProviderRoute};

/// A route that forwards the client's bytes unchanged because the two protocols already agree.
pub struct PassthroughRoute {
    route: Arc<ProviderRoute>,
    endpoint: Endpoint,
}

impl PassthroughRoute {
    /// Builds the front-end for a passthrough route.
    ///
    /// The endpoint is resolved once, here: a route that publishes always addresses a peer, and the
    /// data plane can write the upstream head before it has a body.
    ///
    /// # Errors
    /// Returns [`ExchangeBuildError::NotPassthrough`](crate::ExchangeBuildError::NotPassthrough) when
    /// the route converts,
    /// [`ExchangeBuildError::ProviderMismatch`](crate::ExchangeBuildError::ProviderMismatch) when the
    /// adapter serves another kind,
    /// [`ExchangeBuildError::NativeProtocolMismatch`](crate::ExchangeBuildError::NativeProtocolMismatch)
    /// when the route's client protocol is not the adapter's native one, and
    /// [`ExchangeBuildError::Endpoint`](crate::ExchangeBuildError::Endpoint) when the adapter cannot
    /// compile the route into an addressable endpoint.
    pub fn try_new(
        route: Arc<ProviderRoute>,
        provider: &dyn Provider,
    ) -> Result<Self, crate::ExchangeBuildError> {
        if route.mode() != ExchangeMode::Passthrough {
            return Err(crate::ExchangeBuildError::NotPassthrough {
                route_id: route.route_id().to_owned(),
            });
        }
        if provider.kind() != route.provider_kind() {
            return Err(crate::ExchangeBuildError::ProviderMismatch {
                provider: provider.kind(),
                route: route.provider_kind(),
            });
        }
        if route.protocol() != provider.native_protocol() {
            return Err(crate::ExchangeBuildError::NativeProtocolMismatch {
                route_id: route.route_id().to_owned(),
                provider: provider.native_protocol(),
                route: route.protocol(),
            });
        }
        let endpoint = provider
            .endpoint(&route)
            .map_err(|source| crate::ExchangeBuildError::Endpoint { source })?;
        Ok(Self { route, endpoint })
    }
}

impl LlmRoute for PassthroughRoute {
    /// Always `None`: this route has no answer for "decode this body", so it never has to give one.
    fn converting(&self) -> Option<&dyn ConvertingRoute> {
        None
    }

    fn mode_name(&self) -> &'static str {
        "passthrough"
    }

    fn protocol(&self) -> &str {
        self.route.protocol().as_str()
    }

    fn provider(&self) -> &str {
        self.route.provider_kind().as_str()
    }

    fn destination(&self) -> &Destination {
        self.endpoint.destination()
    }

    fn path(&self) -> &str {
        self.endpoint.path()
    }

    fn headers(&self) -> &[(String, String)] {
        self.endpoint.headers()
    }
}

impl fmt::Debug for PassthroughRoute {
    /// Prints the route identity and its endpoint: both `Debug` implementations name headers and never
    /// print their values, so a credential has no spelling here.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PassthroughRoute")
            .field("route", &self.route)
            .field("endpoint", &self.endpoint)
            .finish()
    }
}
