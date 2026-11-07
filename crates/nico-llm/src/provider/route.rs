//! The compiled route facts one provider adapter reads.
//!
//! A [`ProviderRoute`] is built once, when a configuration document is published, and is read-only
//! afterwards. It answers every question an adapter has that is *not* answered by the canonical
//! request: which upstream to address, which credential to present, which physical model a logical
//! name resolves to, and whether the route converts or passes bytes through. Keeping these out of
//! the canonical request is what lets one request value be re-encoded for a different route without
//! rewriting it.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use super::ProviderKind;
use crate::{
    protocol::ProtocolId,
    schema::{ModelId, ModelRef, SchemaError},
};

/// Which direction an AI route moves bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ExchangeMode {
    /// Decode the client body into the canonical request and re-encode it for the provider.
    #[default]
    Convert,
    /// Forward the client's bytes unchanged, because the client protocol is the provider's own.
    Passthrough,
}

/// A provider credential, materialized once at publication.
///
/// The type exists for one invariant: the secret is never printed. A route is logged at publication
/// and may be logged again per request, so a credential that derived `Debug` could put a provider key
/// into a log line. Reading the value requires naming [`Credential::expose`], which makes every read
/// visible in review.
#[derive(Clone)]
pub struct Credential(String);

impl Credential {
    /// Wraps a materialized secret.
    #[must_use]
    pub fn new(secret: impl Into<String>) -> Self {
        Self(secret.into())
    }

    /// Returns the secret.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Credential {
    /// Prints a placeholder, never the secret.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Credential(<redacted>)")
    }
}

/// Why a [`ProviderRoute`] could not be built.
///
/// This is a **publication** error: the document is refused and the previously published generation
/// keeps serving. Neither variant carries a credential or a request body.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RouteError {
    /// A required identity field is empty, so the route would address nothing.
    #[error("`{field}` must not be empty")]
    Empty {
        /// Dotted path of the empty field.
        field: &'static str,
    },
    /// A `model_map` entry does not name a model, so the map could not resolve anything.
    #[error("`model_map` entry `{name}` is not a valid model")]
    InvalidModel {
        /// The rejected name, as the document wrote it.
        name: String,
        /// The validation error that rejected it.
        #[source]
        source: SchemaError,
    },
}

/// The route facts a provider adapter reads, compiled at publication.
///
/// The identity fields ([`ProviderRoute::route_id`], [`ProviderRoute::protocol`],
/// [`ProviderRoute::provider_kind`]) are how a publication pairs a document route with a codec and an
/// adapter; the operational fields ([`ProviderRoute::base_url`], [`ProviderRoute::credential`],
/// [`ProviderRoute::model_map`]) are what an adapter needs to encode a request. The type is shared
/// through an `Arc`, so a request clones a pointer, not a model map.
pub struct ProviderRoute {
    route_id: String,
    protocol: ProtocolId,
    provider_kind: ProviderKind,
    base_url: String,
    credential: Option<Credential>,
    model_map: BTreeMap<ModelId, ModelId>,
    model_allowlist: BTreeSet<ModelId>,
    mode: ExchangeMode,
}

impl ProviderRoute {
    /// Builds a route that converts and maps no model.
    ///
    /// # Errors
    /// Returns [`RouteError::Empty`] when `route_id` or `base_url` is empty or only whitespace.
    pub fn try_new(
        route_id: impl Into<String>,
        protocol: ProtocolId,
        provider_kind: ProviderKind,
        base_url: impl Into<String>,
    ) -> Result<Self, RouteError> {
        let route_id = route_id.into();
        if route_id.trim().is_empty() {
            return Err(RouteError::Empty { field: "route.id" });
        }
        let base_url = base_url.into();
        if base_url.trim().is_empty() {
            return Err(RouteError::Empty {
                field: "provider.base_url",
            });
        }
        Ok(Self {
            route_id,
            protocol,
            provider_kind,
            base_url,
            credential: None,
            model_map: BTreeMap::new(),
            model_allowlist: BTreeSet::new(),
            mode: ExchangeMode::Convert,
        })
    }

    /// Sets how the route moves bytes; [`ExchangeMode::Convert`] is the default.
    #[must_use]
    pub const fn with_mode(mut self, mode: ExchangeMode) -> Self {
        self.mode = mode;
        self
    }

    /// Attaches the materialized credential the adapter presents upstream.
    #[must_use]
    pub fn with_credential(mut self, credential: Credential) -> Self {
        self.credential = Some(credential);
        self
    }

    /// Replaces the logical-to-physical model map.
    ///
    /// Both names are parsed once, here, so resolving a model at request time is a map lookup that
    /// cannot fail.
    ///
    /// # Errors
    /// Returns [`RouteError::InvalidModel`] when either side of an entry is not a model name.
    pub fn with_model_map(
        mut self,
        model_map: BTreeMap<String, String>,
    ) -> Result<Self, RouteError> {
        let mut parsed = BTreeMap::new();
        for (logical, physical) in model_map {
            let logical_id =
                ModelId::try_new(logical.clone()).map_err(|source| RouteError::InvalidModel {
                    name: logical,
                    source,
                })?;
            let physical_id =
                ModelId::try_new(physical.clone()).map_err(|source| RouteError::InvalidModel {
                    name: physical,
                    source,
                })?;
            parsed.insert(logical_id, physical_id);
        }
        self.model_map = parsed;
        Ok(self)
    }

    /// Restricts the route to the physical models it may serve.
    ///
    /// The names are the **provider's** — what the request will reach the vendor as — because that is
    /// what an operator reads out of a vendor's catalogue, and `model_map` exists precisely so a caller
    /// can write an alias instead. The check therefore runs on the resolved name, not on the one the
    /// caller wrote: `alias -> vendor-model` with `[vendor-model]` is allowed, and the same route with
    /// `[other-model]` is not.
    ///
    /// An empty list means no restriction, which is what a document that omits the field gets.
    ///
    /// # Errors
    /// Returns [`RouteError::InvalidModel`] when an entry is not a model name.
    pub fn with_model_allowlist(mut self, allowlist: Vec<String>) -> Result<Self, RouteError> {
        let mut parsed = BTreeSet::new();
        for name in allowlist {
            let id = ModelId::try_new(name.clone())
                .map_err(|source| RouteError::InvalidModel { name, source })?;
            parsed.insert(id);
        }
        self.model_allowlist = parsed;
        Ok(self)
    }

    /// Reports whether this route may serve the resolved model.
    ///
    /// Membership, not a ranking: an empty list allows everything, so a route that declares nothing
    /// keeps passing names through as before.
    #[must_use]
    pub fn allows(&self, physical: &ModelId) -> bool {
        self.model_allowlist.is_empty() || self.model_allowlist.contains(physical)
    }

    /// Resolves the name the client wrote to the name the provider serves.
    ///
    /// A route that maps nothing passes the name through unchanged, so a model keeps working until
    /// the route renames it. The pair keeps both names because the logical one is what the client and
    /// the log use, and the physical one is what the provider receives.
    #[must_use]
    pub fn model(&self, logical: &ModelId) -> ModelRef {
        let physical = self.model_map.get(logical).unwrap_or(logical);
        ModelRef::new(logical.clone(), physical.clone())
    }

    /// Returns the compiled route identity.
    #[must_use]
    pub fn route_id(&self) -> &str {
        &self.route_id
    }

    /// Returns the client protocol the route is served with.
    #[must_use]
    pub const fn protocol(&self) -> ProtocolId {
        self.protocol
    }

    /// Returns the provider kind that serves the route.
    #[must_use]
    pub const fn provider_kind(&self) -> ProviderKind {
        self.provider_kind
    }

    /// Returns the configured base URL; parsing it into a destination is the adapter's job.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Returns the materialized credential, when the route presents one.
    #[must_use]
    pub const fn credential(&self) -> Option<&Credential> {
        self.credential.as_ref()
    }

    /// Returns the logical-to-physical model map.
    #[must_use]
    pub const fn model_map(&self) -> &BTreeMap<ModelId, ModelId> {
        &self.model_map
    }

    /// Returns how the route moves bytes.
    #[must_use]
    pub const fn mode(&self) -> ExchangeMode {
        self.mode
    }
}

impl fmt::Debug for ProviderRoute {
    /// Prints whether a credential is present, never its value.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderRoute")
            .field("route_id", &self.route_id)
            .field("protocol", &self.protocol)
            .field("provider_kind", &self.provider_kind)
            .field("base_url", &self.base_url)
            .field("credential", &self.credential.is_some())
            .field("model_map", &self.model_map)
            .field("mode", &self.mode)
            .finish()
    }
}
