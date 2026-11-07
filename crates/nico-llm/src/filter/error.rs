//! The exchange's failure vocabulary: one publication failure and one request failure.
//!
//! Both values are the only failures the exchange can produce. A route, a codec and an adapter that
//! cannot be paired is a **publication** failure, refused while the document is compiled; a request
//! that cannot be exchanged is a value the client codec renders in the client's own protocol. No
//! parser diagnostic, provider body or credential crosses either boundary.

use nico_core::ErrorCategory;

use crate::{
    protocol::{Capability, CodecError, ProtocolId},
    provider::{ProviderError, ProviderKind},
    schema::{CanonicalError, ModelId, SchemaError},
};

/// Why a route, a codec and a provider adapter cannot be assembled into a front-end.
///
/// Every variant is a **publication** failure: the document is refused and the previously published
/// generation keeps serving. None of them can be caused by a request. The converting and the
/// passthrough constructors share the vocabulary because both pair a route with an adapter; the two
/// mode-specific variants ([`Self::NotConverted`], [`Self::NotPassthrough`]) are mirror images.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ExchangeBuildError {
    /// The codec speaks a protocol other than the one the route names.
    #[error("the codec speaks `{codec}` but the route names `{route}`")]
    ProtocolMismatch {
        /// The protocol the codec implements.
        codec: ProtocolId,
        /// The protocol the route names.
        route: ProtocolId,
    },
    /// The provider adapter serves a kind other than the one the route names.
    #[error("the provider serves `{provider}` but the route names `{route}`")]
    ProviderMismatch {
        /// The kind the adapter implements.
        provider: ProviderKind,
        /// The kind the route names.
        route: ProviderKind,
    },
    /// The route passes bytes through, so there is no canonical value to exchange.
    #[error("route `{route_id}` is passthrough and has no canonical exchange")]
    NotConverted {
        /// The route that asked for passthrough.
        route_id: String,
    },
    /// The route converts, so it has no passthrough front-end.
    ///
    /// The mirror image of [`Self::NotConverted`]: a front-end is built for the mode its route
    /// declares, and building one for the other mode would either decode bytes the operator asked to
    /// forward or forward bytes nobody encoded.
    #[error("route `{route_id}` converts and has no passthrough front-end")]
    NotPassthrough {
        /// The route that asked to convert.
        route_id: String,
    },
    /// The adapter speaks a protocol other than the one the route passes through byte for byte.
    ///
    /// A passthrough route forwards the client's bytes to the provider unchanged, so the two sides
    /// must speak the same protocol. The adapter answers with its own native protocol instead of the
    /// plan compiler consulting a vendor table.
    #[error("the provider speaks `{provider}` but route `{route_id}` passes through `{route}`")]
    NativeProtocolMismatch {
        /// The route that passes bytes through.
        route_id: String,
        /// The protocol the adapter speaks natively.
        provider: ProtocolId,
        /// The client protocol the route names.
        route: ProtocolId,
    },
    /// The adapter cannot compile the route into an endpoint.
    ///
    /// The failure is static — [`Provider::endpoint`](crate::Provider::endpoint) reads route facts
    /// only — so an unusable `base_url`, a capability the adapter lacks, or a route fact the adapter
    /// must have before it can produce bytes (`EndpointError::RouteFact`; the physical model a path
    /// names, or the output ceiling a dialect requires) is refused while the document is compiled, and
    /// the previously published generation keeps serving. Without this check the route would answer
    /// `500` on every one of its requests instead. A refused route fact keeps the last known good
    /// publication in service like every other publication failure.
    #[error("the route does not address an endpoint: {source}")]
    Endpoint {
        /// The adapter's reason.
        #[source]
        source: ProviderError,
    },
}

/// A failed exchange.
///
/// The value is the exchange's only failure vocabulary: the data plane logs
/// [`ExchangeError::category`] and, when it still can, asks the codec to render
/// [`ExchangeError::canonical`]. The provider's own error body is never part of it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ExchangeError {
    /// The request needs a capability the codec and the provider do not share.
    #[error("this route does not support `{capability}`")]
    Unsupported {
        /// The capability the request needs.
        capability: Capability,
    },
    /// The resolved model is not one this route serves.
    ///
    /// Two request-time policies produce it, and both compare the *resolved* name before anything is
    /// encoded: the operator's `model_allowlist` refused it, or the endpoint's path already names a
    /// different physical model (an endpoint whose request target carries the model cannot take it
    /// from the body). The name here is endpoint configuration: it reaches the log and never the
    /// client's answer, because the map exists to keep the provider's catalogue out of the caller's
    /// vocabulary.
    #[error("the route does not serve the model `{model}`")]
    ModelNotAllowed {
        /// The physical model the route resolved to.
        model: ModelId,
    },
    /// A canonical value crossed the boundary without satisfying its invariants.
    ///
    /// Every canonical shape validates at construction, so this means a codec produced a value that
    /// skipped its constructors — a defect in this crate, not in the client's document.
    #[error("a canonical value is not valid: {source}")]
    InvalidCanonical {
        /// The invariant that failed.
        #[source]
        source: SchemaError,
    },
    /// The client codec could not read the request or write the client-facing bytes.
    #[error(transparent)]
    Codec(#[from] CodecError),
    /// The provider adapter could not reach a canonical value.
    #[error(transparent)]
    Provider(#[from] ProviderError),
}

impl ExchangeError {
    /// Returns the protocol-independent description a client codec renders.
    ///
    /// The code, message and status are compile-time constants: the client-facing text never quotes
    /// a parser diagnostic, a provider body or a credential, and the mapping cannot itself fail.
    #[must_use]
    pub fn canonical(&self) -> CanonicalError {
        match self {
            Self::Unsupported { .. }
            | Self::Codec(CodecError::Unsupported { .. } | CodecError::UnsupportedField { .. })
            | Self::Provider(
                ProviderError::Unsupported { .. } | ProviderError::UnsupportedField { .. },
            ) => CanonicalError::from_static_parts(
                ErrorCategory::Client,
                "unsupported_capability",
                "the route does not support a capability this request needs",
                422,
            ),
            Self::ModelNotAllowed { .. } => CanonicalError::from_static_parts(
                ErrorCategory::Client,
                "model_not_allowed",
                "the route does not serve this model",
                403,
            ),
            Self::Codec(CodecError::InvalidRequest { .. }) => CanonicalError::from_static_parts(
                ErrorCategory::Client,
                "invalid_request",
                "the request is not valid for this protocol",
                400,
            ),
            Self::Codec(CodecError::Encode { .. }) => CanonicalError::from_static_parts(
                ErrorCategory::Internal,
                "client_encode_failed",
                "the gateway could not encode the response",
                500,
            ),
            Self::Provider(ProviderError::InvalidRequest { .. }) => {
                CanonicalError::from_static_parts(
                    ErrorCategory::Internal,
                    "provider_encode_failed",
                    "the gateway could not encode the upstream request",
                    500,
                )
            }
            Self::Provider(ProviderError::InvalidEndpoint { .. }) => {
                CanonicalError::from_static_parts(
                    ErrorCategory::Internal,
                    "upstream_endpoint_invalid",
                    "the upstream endpoint is not configured correctly",
                    500,
                )
            }
            Self::Provider(ProviderError::Malformed { .. }) => CanonicalError::from_static_parts(
                ErrorCategory::Upstream,
                "upstream_protocol_error",
                "the upstream did not return a response this gateway understands",
                502,
            ),
            Self::Provider(ProviderError::Upstream { error }) => error.clone(),
            Self::InvalidCanonical { .. } => CanonicalError::from_static_parts(
                ErrorCategory::Internal,
                "internal_error",
                "the gateway could not process the request",
                500,
            ),
        }
    }

    /// Returns how this failure is classified, for the request record.
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        self.canonical().category()
    }
}
