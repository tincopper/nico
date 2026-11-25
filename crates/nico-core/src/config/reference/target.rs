//! Route target ownership: which fields are meaningful for each target kind.

use super::super::{error::DocumentError, route::RouteTarget};

/// Fields a route wrote that its target kind may or may not own.
pub(super) struct WrittenFields<'a> {
    pub(super) rewrite_prefix: bool,
    pub(super) proxy_buffering: bool,
    pub(super) max_buffer_size: bool,
    pub(super) proxy_set_header: bool,
    pub(super) proxy_del_header: bool,
    pub(super) protocol: Option<&'a str>,
    pub(super) mode: Option<super::super::route::RouteMode>,
    pub(super) max_response_bytes: Option<u32>,
    pub(super) fallback: bool,
    pub(super) ttft_timeout: bool,
    pub(super) token_budget: bool,
}

/// Rejects a field that the route's target kind does not own, and a provider route without a
/// protocol.
pub(super) fn check_target_fields(
    target: &RouteTarget,
    written: WrittenFields<'_>,
) -> Result<(), DocumentError> {
    match target {
        RouteTarget::Upstream(_) => {
            let declared = [
                ("routes[].protocol", written.protocol.is_some()),
                ("routes[].mode", written.mode.is_some()),
                (
                    "routes[].max_response_bytes",
                    written.max_response_bytes.is_some(),
                ),
                ("routes[].fallback", written.fallback),
                ("routes[].ttft_timeout_ms", written.ttft_timeout),
                ("routes[].token_budget", written.token_budget),
            ];
            for (field, present) in declared {
                if present {
                    return Err(DocumentError::InvalidField {
                        field,
                        reason: "is only valid on a provider route".into(),
                    });
                }
            }
        }
        RouteTarget::Provider(_) => {
            if written.rewrite_prefix {
                return Err(DocumentError::InvalidField {
                    field: "routes[].rewrite_prefix",
                    reason: "is only valid on an upstream route: a converting route's path belongs
                             to its adapter"
                        .into(),
                });
            }
            for (field, present) in [
                ("routes[].proxy_buffering", written.proxy_buffering),
                ("routes[].max_buffer_size", written.max_buffer_size),
                ("routes[].proxy_set_header", written.proxy_set_header),
                ("routes[].proxy_del_header", written.proxy_del_header),
            ] {
                if present {
                    return Err(DocumentError::InvalidField {
                        field,
                        reason: "is only valid on an upstream route: a converting route's request
                                 and response belong to its adapter"
                            .into(),
                    });
                }
            }
            if written.protocol.is_none() {
                return Err(DocumentError::InvalidField {
                    field: "routes[].protocol",
                    reason: "is required on a provider route".into(),
                });
            }
        }
    }
    Ok(())
}
