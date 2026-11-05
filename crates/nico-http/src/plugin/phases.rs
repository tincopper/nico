//! The stages a plugin declares it uses.
//!
//! The driver calls every stage of every plugin in its scope: the default implementations continue,
//! so a plugin that declares nothing still runs, and a plugin that forgets to declare a stage is not
//! silently skipped. The declaration therefore answers exactly one question the data plane cannot
//! re-derive later: **how may the response be framed?**
//!
//! A plugin that rewrites the response body per chunk makes a length-framed response impossible to
//! honour — the number the upstream declared describes bytes that are no longer the bytes being sent.
//! The publication records that decision per route, and the response-header hook drops the upstream
//! `content-length` for such a route so Pingora frames the response as chunked instead.
//!
//! The asymmetry between the two ways of getting it wrong is worth stating, because it is what the
//! doc comment on [`super::Plugin::on_response_body`] has to warn about:
//!
//! - **Under-declaring** [`PluginPhases::RESPONSE_BODY`] desynchronises a `content-length` response,
//!   so a kind that rewrites bytes must declare it.
//! - **Over-declaring** costs a chunked response, which every client accepts.
//!
//! The set grows by adding constants, which is not a breaking change; a match on a value of this type
//! would be, so there is no way to destructure it.

use std::fmt;

/// The set of stages one plugin implements.
///
/// Values are combined with `|`, for example `PluginPhases::REQUEST | PluginPhases::RESPONSE`, and
/// tested with [`PluginPhases::contains`].
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct PluginPhases(u8);

impl PluginPhases {
    /// Declares no stage.
    ///
    /// A plugin that declares this still runs every stage — the declaration is about framing, not
    /// about whether the plugin is called — but the data plane assumes it changes no response bytes.
    pub const NONE: Self = Self(0);

    /// `on_request`: request headers and the request target.
    pub const REQUEST: Self = Self(1 << 0);

    /// `on_request_body`: the request body, chunk by chunk or whole.
    pub const REQUEST_BODY: Self = Self(1 << 1);

    /// `on_response`: the response status and headers.
    pub const RESPONSE: Self = Self(1 << 2);

    /// `on_response_body`: the response body, always chunk by chunk.
    ///
    /// This is the one flag with a consequence outside the plugin: a route whose chain declares it
    /// cannot keep the upstream's `content-length`, so its response is framed as chunked.
    pub const RESPONSE_BODY: Self = Self(1 << 3);

    /// Returns whether every stage in `other` is in this set.
    ///
    /// `contains(PluginPhases::NONE)` is always true, because an empty requirement is always met.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Returns whether no stage is declared.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for PluginPhases {
    type Output = Self;

    /// Returns the union of two declarations.
    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl fmt::Debug for PluginPhases {
    /// Prints the stage names, because the value is written to the publication log as text.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut set = formatter.debug_set();
        for (flag, name) in [
            (Self::REQUEST, "request"),
            (Self::REQUEST_BODY, "request_body"),
            (Self::RESPONSE, "response"),
            (Self::RESPONSE_BODY, "response_body"),
        ] {
            if self.contains(flag) {
                set.entry(&name);
            }
        }
        set.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::PluginPhases;

    #[test]
    fn a_declaration_combines_with_or_and_reports_membership() {
        let phases = PluginPhases::REQUEST | PluginPhases::RESPONSE_BODY;

        assert!(phases.contains(PluginPhases::REQUEST));
        assert!(phases.contains(PluginPhases::RESPONSE_BODY));
        assert!(!phases.contains(PluginPhases::RESPONSE));
        assert!(!phases.contains(PluginPhases::REQUEST_BODY));
    }

    #[test]
    fn an_empty_requirement_is_always_met() {
        assert!(PluginPhases::NONE.contains(PluginPhases::NONE));
        assert!(PluginPhases::RESPONSE.contains(PluginPhases::NONE));
        assert!(PluginPhases::NONE.is_empty());
        assert!(!PluginPhases::REQUEST.is_empty());
    }

    #[test]
    fn the_debug_form_names_the_stages_in_their_execution_order() {
        let phases = PluginPhases::RESPONSE_BODY | PluginPhases::REQUEST;

        assert_eq!(format!("{phases:?}"), r#"{"request", "response_body"}"#);
        assert_eq!(format!("{:?}", PluginPhases::NONE), "{}");
    }
}
