//! The OpenAI adapter's operation paths and headers over the shared base-URL rules.
//!
//! The parsing rules — scheme, authority, port, host, prefix and the refusals — live in
//! [`crate::provider::base_url`] because they are the same for every provider. What belongs to this
//! adapter is only *which paths* the two OpenAI wire dialects use: chat completions on a route that
//! names an API root, and the same prefix with `/chat/completions` appended on a route that names a
//! prefix. A second dialect (Responses) reuses [`operation`] with its own pair.
//!
//! `accept` names both shapes one endpoint answers: the same path returns a complete document or an
//! event stream depending on a field of the request body, and the endpoint may not read the body, so
//! the adapter advertises both instead of guessing.

use super::Endpoint;
use crate::provider::{EndpointError, ProviderRoute, base_url};

/// The chat path of a base URL that names no path of its own.
pub(super) const DEFAULT_REQUEST_PATH: &str = "/v1/chat/completions";

/// The path appended to a base URL that names an API prefix.
pub(super) const CHAT_PATH: &str = "/chat/completions";

/// Builds the endpoint the data plane will contact for one chat route.
///
/// # Errors
/// Returns [`EndpointError::BaseUrl`] when the route's `base_url` is not an addressable URL, and the
/// [`Endpoint`] checks' own variants when a derived path or header is not sendable.
pub(super) fn endpoint(route: &ProviderRoute) -> Result<Endpoint, EndpointError> {
    operation(route, DEFAULT_REQUEST_PATH, CHAT_PATH)
}

/// Builds an endpoint for one path pair of this vendor's API.
///
/// `default_path` is what an API root means for the operation and `suffix` is what a prefix gains;
/// the pair is a constant of the adapter, never a request value. It is visible to this vendor's
/// sibling dialect module for exactly that reason: a path pair is the only thing each dialect owns
/// here, and the parsing rules and header set are shared.
pub(in crate::provider) fn operation(
    route: &ProviderRoute,
    default_path: &str,
    suffix: &str,
) -> Result<Endpoint, EndpointError> {
    let base = base_url::parse(route.base_url())?;
    let path = base.path(default_path, suffix);
    Endpoint::try_new(base.destination, path, headers(route))
}

/// Builds the headers the provider adds to the request.
fn headers(route: &ProviderRoute) -> Vec<(String, String)> {
    let mut headers = vec![
        ("content-type".to_owned(), "application/json".to_owned()),
        (
            "accept".to_owned(),
            "application/json, text/event-stream".to_owned(),
        ),
    ];
    if let Some(credential) = route.credential() {
        headers.push((
            "authorization".to_owned(),
            format!("Bearer {}", credential.expose()),
        ));
    }
    headers
}
