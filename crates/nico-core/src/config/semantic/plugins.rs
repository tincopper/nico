//! Semantic validation of one `plugins` entry — the **envelope**, and only the envelope.
//!
//! Every rule here is local to one entry — the name a route references, the rejection status, the
//! rejection content type — so a failure names the field it came from instead of the route that
//! references it. Whether a route names a plugin that exists is `super::super::reference`'s job.
//!
//! Nothing here knows a kind. `plugins[].kind` is a non-empty string and the meaning of
//! `plugins[].config` is decided by the crate that implements the kind, which validates it while the
//! document is compiled: same stage, same last-known-good behaviour, but the rules live with the code
//! that has to honour them. A range check here would be a second copy of a rule the implementation
//! also has to enforce, and the two would eventually disagree.
//!
//! One envelope rule is not repeated here because the type makes it unrepresentable:
//! [`PluginSpec`](crate::config::PluginSpec) values are only built by
//! [`spec_from_raw`](crate::config::plugin), which refuses an empty `kind`.

use super::{invalid, validate_content_type};
use crate::config::{error::DocumentError, plugin::PluginSpec};

/// Checks every plugin entry, in document order.
///
/// # Errors
/// Returns [`DocumentError::InvalidField`] for a malformed plugin name, a rejection status outside
/// 4xx, or a rejection content type that is not a printable `type/subtype` pair. A repeated name is
/// not reported here: uniqueness spans the whole collection and belongs to
/// `super::super::reference`, which also checks the references that point at these names.
pub(super) fn validate(plugins: &[PluginSpec]) -> Result<(), DocumentError> {
    for plugin in plugins {
        super::validate_name("plugins[].name", plugin.name())?;
        validate_error_response(plugin)?;
    }
    Ok(())
}

/// Checks a rejection presentation: the status range, the content type, and nothing else.
///
/// The body's own shape was decided when the entry was built, because only the content type knows
/// whether it must be structured or a literal string.
fn validate_error_response(plugin: &PluginSpec) -> Result<(), DocumentError> {
    const FIELD: &str = "plugins[].error_response.content_type";
    let Some(response) = plugin.error_response() else {
        return Ok(());
    };
    if let Some(status) = response.status()
        && !super::REJECTION_STATUS.contains(&status)
    {
        return Err(invalid(
            "plugins[].error_response.status",
            format!(
                "must be in {}..={}",
                super::REJECTION_STATUS.start(),
                super::REJECTION_STATUS.end()
            ),
        ));
    }
    validate_content_type(FIELD, response.content_type())
}
