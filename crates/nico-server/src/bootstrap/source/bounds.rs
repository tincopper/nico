//! `config_source.schema_bounds`: how large a document this process is willing to parse.
//!
//! Every field is optional, so the whole block or any single field can be omitted. A field that is
//! absent keeps its [`SchemaBounds::default`] value; a field that is *present* must be accepted as
//! written, so a typo or a `0` fails at startup instead of being absorbed by the default. The
//! built-in values are never restated here: an omitted field is simply not applied.
//!
//! The bounds live beside the source section rather than inside it because they describe the shape
//! of the *document* every source type produces, and they keep their meaning and their rules
//! exactly as they had them before the section existed.

use nico_core::{Bound, SchemaBounds};

use crate::bootstrap::BootstrapError;

/// Largest accepted `config_source.schema_bounds.max_routes`, `max_upstreams`, `max_providers` and
/// `max_plugins`.
const MAX_COLLECTION_BOUND: usize = 1_000_000;

/// Largest accepted `config_source.schema_bounds.max_nodes`.
const MAX_NODES_BOUND: usize = 10_000_000;

/// Largest accepted `config_source.schema_bounds.max_depth`.
const MAX_DEPTH_BOUND: usize = 128;

/// The optional `config_source.schema_bounds` block exactly as written.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawSchemaBounds {
    max_routes: Option<usize>,
    max_upstreams: Option<usize>,
    max_providers: Option<usize>,
    max_plugins: Option<usize>,
    max_endpoints: Option<usize>,
    max_pool_members: Option<usize>,
    max_nodes: Option<usize>,
    max_depth: Option<usize>,
}

impl RawSchemaBounds {
    /// Applies every written bound over the defaults, rejecting one outside its operator range.
    ///
    /// # Errors
    /// Returns [`BootstrapError::InvalidField`] naming `config_source.schema_bounds.<bound>` when a
    /// written value is zero or above the limit an operator may write.
    pub(super) fn resolve(&self) -> Result<SchemaBounds, BootstrapError> {
        let mut bounds = SchemaBounds::default();
        for (bound, written) in [
            (Bound::Routes, self.max_routes),
            (Bound::Upstreams, self.max_upstreams),
            (Bound::Providers, self.max_providers),
            (Bound::Plugins, self.max_plugins),
            (Bound::Endpoints, self.max_endpoints),
            (Bound::PoolMembers, self.max_pool_members),
            (Bound::Nodes, self.max_nodes),
            (Bound::Depth, self.max_depth),
        ] {
            let Some(value) = written else { continue };
            if value > maximum(bound) {
                return Err(out_of_range(bound, value));
            }
            bounds = bounds
                .with(bound, value)
                // The upper bound was checked above, so `with` can only reject a `0` here.
                .map_err(|_error| out_of_range(bound, value))?;
        }
        Ok(bounds)
    }
}

/// Returns the largest value an operator may write for `bound`.
///
/// This is a bootstrap policy, not a property of [`SchemaBounds`]: the parser accepts any non-zero
/// bound, while a hand-written file is bounded so a typo cannot ask for a million routes. The match
/// is exhaustive on purpose — a new bound must state its own limit instead of inheriting one.
pub(crate) const fn maximum(bound: Bound) -> usize {
    match bound {
        Bound::Routes
        | Bound::Upstreams
        | Bound::Providers
        | Bound::Plugins
        | Bound::Endpoints
        | Bound::PoolMembers => MAX_COLLECTION_BOUND,
        Bound::Nodes => MAX_NODES_BOUND,
        Bound::Depth => MAX_DEPTH_BOUND,
    }
}

/// Builds the error for a bound an operator may not write.
fn out_of_range(bound: Bound, value: usize) -> BootstrapError {
    BootstrapError::InvalidField {
        field: format!("config_source.schema_bounds.{}", bound.name()).into(),
        reason: format!("must be in 1..={} (found {value})", maximum(bound)).into(),
    }
}
