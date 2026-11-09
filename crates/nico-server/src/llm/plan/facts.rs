//! One provider entry, compiled into the route facts its adapter reads.
//!
//! The boundary is "a document's endpoint becomes a `ProviderRoute`": the base URL as written, the
//! materialized credential, the model map in both directions, and the entry's own output ceiling. It
//! sits in its own module because it is the one place a provider **entry** is read, and every route of
//! that entry — a primary and each fallback member — compiles through it, so a change to what an entry
//! contributes cannot reach one of them and miss another.
//!
//! Nothing here opens a file or reads the environment: the credential arrives already materialized,
//! which is what keeps the one-read-per-provider rule visible in the caller instead of hiding a second
//! read behind this call.

use std::num::NonZeroU32;

use nico_core::{Document, Provider};
use nico_llm::{Credential, ExchangeMode, ProtocolId, ProviderKind, ProviderRoute, RouteError};

use crate::llm::LlmBuildError;

/// Returns the document's provider entry named `name`.
pub(crate) fn provider_entry<'a>(document: &'a Document, name: &str) -> Option<&'a Provider> {
    document
        .providers()
        .iter()
        .find(|provider| provider.name() == name)
}

/// Compiles one provider entry into the route facts its adapter reads.
///
/// Every value here is read once per publication: the base URL as written, the materialized
/// credential, the model map in both directions, and the endpoint's own output ceiling. `route_id`
/// only names the route in a refusal, so it is not stored twice.
///
/// # Errors
/// Returns [`LlmBuildError::Route`] when the entry cannot form route facts: an empty base URL, a model
/// name that is not a name, or an output ceiling of zero.
pub(crate) fn route_facts(
    route_id: &str,
    protocol: ProtocolId,
    provider_kind: ProviderKind,
    entry: &Provider,
    credential: Credential,
    mode: ExchangeMode,
) -> Result<ProviderRoute, LlmBuildError> {
    let to_build_error = |source| LlmBuildError::Route {
        route: route_id.to_owned(),
        source,
    };
    let facts = ProviderRoute::try_new(route_id, protocol, provider_kind, entry.base_url())
        .map_err(to_build_error)?
        .with_mode(mode)
        .with_credential(credential)
        .with_model_map(entry.model_map().clone())
        .map_err(to_build_error)?
        .with_model_allowlist(entry.model_allowlist().to_vec())
        .map_err(to_build_error)?;
    // The document already refuses a ceiling of zero, so a `None` here means the entry sets none. The
    // conversion is still spelled out rather than unwrapped: facts built from a document that skipped
    // its own validation must refuse themselves, not panic during a publication.
    let max_output_tokens = entry
        .max_output_tokens()
        .map(|value| {
            NonZeroU32::new(value).ok_or_else(|| {
                to_build_error(RouteError::Empty {
                    field: "providers[].max_output_tokens",
                })
            })
        })
        .transpose()?;
    Ok(match max_output_tokens {
        Some(ceiling) => facts.with_max_output_tokens(ceiling),
        None => facts,
    })
}
