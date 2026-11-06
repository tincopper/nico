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
    schema::{CanonicalError, SchemaError},
};

/// Why a route, a codec and a provider adapter cannot be paired.
///
/// Every variant is a **publication** failure: the document is refused and the previously published
/// generation keeps serving. None of them can be caused by a request.
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
            | Self::Codec(CodecError::Unsupported { .. })
            | Self::Provider(ProviderError::Unsupported { .. }) => {
                CanonicalError::from_static_parts(
                    ErrorCategory::Client,
                    "unsupported_capability",
                    "the route does not support a capability this request needs",
                    422,
                )
            }
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
