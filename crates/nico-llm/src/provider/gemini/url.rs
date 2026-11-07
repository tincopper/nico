//! The `generateContent` endpoint: the shared base-URL rules plus this protocol's path and headers.
//!
//! The request path names two values that would otherwise come from the body — the physical model and
//! the operation — and the upstream head is written before the body is decoded, so both are settled
//! here, from route facts only. The model must be the route's unique `model_map` value and is then
//! recorded on the endpoint, so the exchange can refuse a request that resolved elsewhere; the
//! operation is always the streaming one, which is why this adapter folds a stream into a complete
//! answer.
//!
//! The parse is hand-written over the shared [`base_url`] rules: the model is written **literally**
//! into a path segment, so a name that would need escaping is refused rather than escaped. A percent
//! escape would make one deployment address a different resource than the operator wrote, and an
//! unescaped `?` or `#` would silently turn part of a model name into a query or fragment.

use super::{
    super::{Endpoint, EndpointError, ExchangeMode, ProviderRoute, base_url},
    VERSION,
};
use crate::schema::ModelId;

/// The dotted configuration field a route fact failure names.
const MODE_FIELD: &str = "routes[].mode";

/// The dotted configuration field a missing or non-constant model names.
const MODEL_FIELD: &str = "providers[].model_map";

/// The operation this adapter pins; the streaming spelling carries `alt=sse` in the query.
const OPERATION: &str = "streamGenerateContent";

/// The query that asks this dialect for an event stream rather than a JSON array.
const ALT: &str = "alt=sse";

/// Builds the endpoint the data plane will contact for one route.
///
/// # Errors
/// Returns [`EndpointError::RouteFact`] when the route asks for passthrough or does not pin one
/// addressable model, and [`EndpointError::BaseUrl`] or the [`Endpoint`] checks when the address is
/// not sendable. All of them are publication failures: the document is refused and the previous
/// generation keeps serving.
pub(super) fn endpoint(route: &ProviderRoute) -> Result<Endpoint, EndpointError> {
    // The route facts are checked first: a route this adapter cannot serve is refused for that
    // reason even when its base URL would also be refused, and the operator reads the specific one.
    reject_passthrough(route)?;
    let physical = pinned_model(route)?;
    let base = base_url::parse(route.base_url())?;
    // The path is the same string with and without a base prefix: this dialect's operation path keeps
    // its own `/v1beta` under a prefix, so an origin gets the operation and a prefix gets it appended.
    let operation = format!("/{VERSION}/models/{physical}:{OPERATION}?{ALT}");
    let path = base.path(&operation, &operation);
    Ok(Endpoint::try_new(base.destination, path, headers(route))?.with_model(physical))
}

/// Refuses a `passthrough` route.
///
/// A passthrough route forwards the caller's bytes to a path fixed at publication, and this dialect's
/// request target carries the model and the operation. Forwarding would therefore send the caller's
/// request to whichever model and operation this route pinned — a different question, answered
/// confidently. Returning the caller's target instead is a data-plane contract change, not a
/// configuration option, so the route is refused here.
fn reject_passthrough(route: &ProviderRoute) -> Result<(), EndpointError> {
    if route.mode() == ExchangeMode::Convert {
        return Ok(());
    }
    Err(EndpointError::RouteFact {
        field: MODE_FIELD,
        reason: "this dialect's request target carries the model and the operation, so fixed \
                 passthrough bytes would reach a different model than the caller named"
            .into(),
    })
}

/// Returns the one physical model this route serves.
///
/// "Constant" is the rule, not "one entry": two aliases that resolve to the same model describe one
/// endpoint, and the path names that model once. A route with no entry is refused because the path
/// would name nothing.
///
/// # Errors
/// Returns [`EndpointError::RouteFact`] when the map is empty, names more than one model, or names a
/// model that cannot be written into a path segment.
pub(super) fn pinned_model(route: &ProviderRoute) -> Result<ModelId, EndpointError> {
    let mut values = route.model_map().values();
    let first = values.next().ok_or_else(|| EndpointError::RouteFact {
        field: MODEL_FIELD,
        reason: "this dialect names the model in the request path, so the route must map every \
                 logical name to one physical model"
            .into(),
    })?;
    if values.any(|physical| physical != first) {
        return Err(EndpointError::RouteFact {
            field: MODEL_FIELD,
            reason: "this dialect names one model in the request path, so every entry of the map \
                     must resolve to the same physical model"
                .into(),
        });
    }
    if !is_path_segment(first.as_str()) {
        return Err(EndpointError::RouteFact {
            field: MODEL_FIELD,
            reason: "the model is written literally into a path segment, so its name may hold only \
                     letters, digits, `.`, `_` and `-`; a name needing an escape or carrying a path \
                     separator would address a different resource than the one written"
                .into(),
        });
    }
    Ok(first.clone())
}

/// Reports whether a model name can be a literal path segment.
fn is_path_segment(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

/// Builds the headers the provider adds to the request.
fn headers(route: &ProviderRoute) -> Vec<(String, String)> {
    let mut headers = vec![
        ("content-type".to_owned(), "application/json".to_owned()),
        ("accept".to_owned(), "text/event-stream".to_owned()),
    ];
    if let Some(credential) = route.credential() {
        headers.push(("x-goog-api-key".to_owned(), credential.expose().to_owned()));
    }
    headers
}
