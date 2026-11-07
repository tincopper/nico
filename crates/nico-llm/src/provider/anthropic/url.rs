//! The Messages endpoint: the shared base-URL rules plus this protocol's path and headers.
//!
//! `/v1/messages` requires `max_tokens` on every request, and the canonical request may omit it, so
//! the endpoint refuses a route that does not answer the question — `providers[].max_output_tokens`
//! is the ceiling a request without one gets. The check runs here, at publication, because the
//! adapter has no later moment at which it could invent a value honestly: a constant would silently
//! cap the caller's answer.
//!
//! The `accept` header names both shapes one path answers, for the same reason the OpenAI adapter
//! does: the stream flag lives in the request body, which the endpoint may not read.

use std::num::NonZeroU32;

use super::VERSION;
use crate::provider::{Endpoint, EndpointError, ProviderRoute, base_url};

/// The Messages path of a base URL that names no path of its own.
const DEFAULT_REQUEST_PATH: &str = "/v1/messages";

/// The path appended to a base URL that names an API prefix.
const MESSAGES_PATH: &str = "/messages";

/// The dotted configuration field a missing output ceiling names.
const CEILING_FIELD: &str = "providers[].max_output_tokens";

/// Builds the endpoint the data plane will contact for one route.
///
/// # Errors
/// Returns [`EndpointError::RouteFact`] when the route names no output ceiling, and
/// [`EndpointError::BaseUrl`] or the [`Endpoint`] checks when the address is not sendable. All three
/// are publication failures: the document is refused and the previous generation keeps serving.
pub(super) fn endpoint(route: &ProviderRoute) -> Result<Endpoint, EndpointError> {
    // The route fact is checked first: a route that cannot answer the dialect's required ceiling is
    // refused whether or not its base URL would also be refused, and the operator reads the reason
    // that is specific to this adapter.
    output_ceiling(route)?;
    let base = base_url::parse(route.base_url())?;
    let path = base.path(DEFAULT_REQUEST_PATH, MESSAGES_PATH);
    Endpoint::try_new(base.destination, path, headers(route))
}

/// Returns the ceiling this endpoint uses when the request names none.
///
/// # Errors
/// Returns [`EndpointError::RouteFact`] when the route names none: this dialect requires the field.
pub(super) fn output_ceiling(route: &ProviderRoute) -> Result<NonZeroU32, EndpointError> {
    route
        .max_output_tokens()
        .ok_or_else(|| EndpointError::RouteFact {
            field: CEILING_FIELD,
            reason: "`/v1/messages` requires `max_tokens` and the route names no default for a \
                     request that omits it"
                .into(),
        })
}

/// Builds the headers the provider adds to the request.
fn headers(route: &ProviderRoute) -> Vec<(String, String)> {
    let mut headers = vec![
        ("content-type".to_owned(), "application/json".to_owned()),
        (
            "accept".to_owned(),
            "application/json, text/event-stream".to_owned(),
        ),
        ("anthropic-version".to_owned(), VERSION.to_owned()),
    ];
    if let Some(credential) = route.credential() {
        headers.push(("x-api-key".to_owned(), credential.expose().to_owned()));
    }
    headers
}
