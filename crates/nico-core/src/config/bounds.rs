//! Parse-time bounds: how large and how complex a configuration document may be.
//!
//! Two independent limits are combined here. [`ParseLimits::max_bytes`] is a transport bound: it
//! says how many bytes a configuration source is allowed to hand over. [`SchemaBounds`] is a shape
//! bound: it says how complex the decoded document may be, so a document that fits in the byte
//! budget still cannot expand into an unbounded graph of routes, upstreams or providers.
//!
//! Every bound is a `NonZeroUsize`, so a zero bound — which would make every document invalid — is
//! unrepresentable. The defaults live in exactly one place, [`SchemaBounds::default`]; operators
//! override them through the local bootstrap file, never through a second copy of the numbers.
//!
//! A bound is addressed by [`Bound`] rather than by a per-field method: the fields differ only in
//! name, so one [`SchemaBounds::get`] and one [`SchemaBounds::with`] keep the table of names in a
//! single place, and adding a bound means adding one variant instead of two dozen lines of
//! copy-pasted accessors.

use std::num::NonZeroUsize;

/// One named field of [`SchemaBounds`].
///
/// [`Bound::name`] is the configuration key the field is written as, so an error message and the
/// bootstrap file name the same thing without a second table of strings.
///
/// The set is closed on purpose. A new bound is a new configuration key, and every consumer must
/// decide what it accepts for it — an operator writes a different range than the parser — so
/// `#[non_exhaustive]` would trade that compiler-enforced update for a silent wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    /// Maximum accepted number of routes.
    Routes,
    /// Maximum accepted number of upstreams.
    Upstreams,
    /// Maximum accepted number of providers.
    Providers,
    /// Maximum accepted number of members of one upstream's `endpoints` list.
    ///
    /// Per entry rather than collection-wide: the list is the candidate set one request's selection
    /// walks, so its size is a per-request cost rather than a document-wide budget.
    Endpoints,
    /// Maximum accepted number of members of one route's `providers` pool.
    ///
    /// Per route rather than collection-wide, for the same reason [`Bound::Endpoints`] is: the list is
    /// the candidate set one attempt's selection walks, and it is also a factor of the route's attempt
    /// ceiling.
    PoolMembers,
    /// Maximum accepted number of plugins.
    Plugins,
    /// Maximum accepted number of YAML nodes in the whole document.
    Nodes,
    /// Maximum accepted nesting depth, where the root sits at depth `1`.
    Depth,
}

impl Bound {
    /// Returns the configuration key of this bound, for example `max_routes`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Routes => "max_routes",
            Self::Upstreams => "max_upstreams",
            Self::Providers => "max_providers",
            Self::Endpoints => "max_endpoints",
            Self::PoolMembers => "max_pool_members",
            Self::Plugins => "max_plugins",
            Self::Nodes => "max_nodes",
            Self::Depth => "max_depth",
        }
    }
}

/// Bounds on the shape of a configuration document.
///
/// Obtain through [`SchemaBounds::default`], then replace individual fields with
/// [`SchemaBounds::with`]; both reject `0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchemaBounds {
    max_routes: NonZeroUsize,
    max_upstreams: NonZeroUsize,
    max_providers: NonZeroUsize,
    max_plugins: NonZeroUsize,
    max_endpoints: NonZeroUsize,
    max_pool_members: NonZeroUsize,
    max_nodes: NonZeroUsize,
    max_depth: NonZeroUsize,
}

impl Default for SchemaBounds {
    /// Returns the built-in bounds.
    ///
    /// These numbers are the single definition of the defaults: the bootstrap file and
    /// [`ParseLimits::new`] both derive from this implementation instead of repeating them.
    fn default() -> Self {
        Self {
            max_routes: default_bound(1024),
            max_upstreams: default_bound(256),
            max_providers: default_bound(64),
            max_plugins: default_bound(256),
            max_endpoints: default_bound(64),
            max_pool_members: default_bound(8),
            max_nodes: default_bound(100_000),
            max_depth: default_bound(32),
        }
    }
}

impl SchemaBounds {
    /// Returns the value of one bound.
    #[must_use]
    pub const fn get(self, bound: Bound) -> usize {
        match bound {
            Bound::Routes => self.max_routes.get(),
            Bound::Upstreams => self.max_upstreams.get(),
            Bound::Providers => self.max_providers.get(),
            Bound::Plugins => self.max_plugins.get(),
            Bound::Endpoints => self.max_endpoints.get(),
            Bound::PoolMembers => self.max_pool_members.get(),
            Bound::Nodes => self.max_nodes.get(),
            Bound::Depth => self.max_depth.get(),
        }
    }

    /// Returns a copy with one bound replaced.
    ///
    /// # Errors
    /// Returns [`BoundsError`] naming the field when `value` is `0`.
    pub fn with(self, bound: Bound, value: usize) -> Result<Self, BoundsError> {
        let value = bound_value(bound, value)?;
        Ok(match bound {
            Bound::Routes => Self {
                max_routes: value,
                ..self
            },
            Bound::Upstreams => Self {
                max_upstreams: value,
                ..self
            },
            Bound::Providers => Self {
                max_providers: value,
                ..self
            },
            Bound::Plugins => Self {
                max_plugins: value,
                ..self
            },
            Bound::Endpoints => Self {
                max_endpoints: value,
                ..self
            },
            Bound::PoolMembers => Self {
                max_pool_members: value,
                ..self
            },
            Bound::Nodes => Self {
                max_nodes: value,
                ..self
            },
            Bound::Depth => Self {
                max_depth: value,
                ..self
            },
        })
    }
}

/// Bounds applied while parsing a configuration document.
///
/// This is the pipeline's only entry point for parse limits: the byte bound comes from the
/// configuration source, the shape bounds from [`SchemaBounds`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseLimits {
    max_bytes: usize,
    schema: SchemaBounds,
}

impl ParseLimits {
    /// Creates parse limits from the maximum accepted document size, with [`SchemaBounds::default`].
    #[must_use]
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            schema: SchemaBounds::default(),
        }
    }

    /// Returns a copy with different shape bounds.
    #[must_use]
    pub const fn with_schema(self, schema: SchemaBounds) -> Self {
        Self {
            max_bytes: self.max_bytes,
            schema,
        }
    }

    /// Returns the maximum accepted document size in bytes.
    #[must_use]
    pub const fn max_bytes(self) -> usize {
        self.max_bytes
    }

    /// Returns the shape bounds applied after the document is decoded.
    #[must_use]
    pub const fn schema(self) -> SchemaBounds {
        self.schema
    }
}

/// A bound that cannot be satisfied by any document, because it was set to `0`.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("bounds value must be at least 1: {field} = {value}")]
pub struct BoundsError {
    field: &'static str,
    value: usize,
}

impl BoundsError {
    /// Returns the rejected field name, for example `max_depth`.
    #[must_use]
    pub const fn field(&self) -> &'static str {
        self.field
    }

    /// Returns the rejected value, always `0`.
    #[must_use]
    pub const fn value(&self) -> usize {
        self.value
    }
}

/// Converts one bound to its non-zero representation.
///
/// # Errors
/// Returns [`BoundsError`] when `value` is `0`.
fn bound_value(bound: Bound, value: usize) -> Result<NonZeroUsize, BoundsError> {
    NonZeroUsize::new(value).ok_or(BoundsError {
        field: bound.name(),
        value,
    })
}

/// Converts one compile-time default into a non-zero bound.
///
/// The defaults are literals greater than zero, so the `None` arm is unreachable for them: it exists
/// so the constructor stays total without an `unwrap` on a non-test path. A default accidentally set
/// to `0` collapses to `1`, which the default-table test catches.
const fn default_bound(value: usize) -> NonZeroUsize {
    match NonZeroUsize::new(value) {
        Some(bound) => bound,
        None => NonZeroUsize::MIN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The documented defaults, keyed by the bound they belong to.
    const DEFAULTS: [(Bound, usize); 7] = [
        (Bound::Routes, 1024),
        (Bound::Upstreams, 256),
        (Bound::Providers, 64),
        (Bound::Plugins, 256),
        (Bound::Endpoints, 64),
        (Bound::Nodes, 100_000),
        (Bound::Depth, 32),
    ];

    #[test]
    fn defaults_match_the_documented_table() {
        let bounds = SchemaBounds::default();
        for (bound, expected) in DEFAULTS {
            assert_eq!(
                bounds.get(bound),
                expected,
                "wrong default for {}",
                bound.name()
            );
        }
    }

    #[test]
    fn every_bound_is_named_after_its_configuration_key() {
        for (bound, _) in DEFAULTS {
            assert!(
                bound.name().starts_with("max_"),
                "{} must name a configuration key",
                bound.name()
            );
        }
    }

    #[test]
    fn with_rejects_zero_for_every_bound() {
        for (bound, _) in DEFAULTS {
            let error = SchemaBounds::default()
                .with(bound, 0)
                .expect_err("a zero bound cannot be satisfied by any document");
            assert_eq!(error.field(), bound.name());
            assert_eq!(error.value(), 0);
            assert_eq!(
                error.to_string(),
                format!("bounds value must be at least 1: {} = 0", bound.name())
            );
        }
    }

    #[test]
    fn with_accepts_one_for_every_bound() {
        let mut bounds = SchemaBounds::default();
        for (bound, _) in DEFAULTS {
            bounds = bounds.with(bound, 1).expect("1 is a legal bound");
        }
        for (bound, _) in DEFAULTS {
            assert_eq!(bounds.get(bound), 1, "{} was not replaced", bound.name());
        }
    }

    #[test]
    fn with_replaces_only_the_named_bound() {
        let bounds = SchemaBounds::default()
            .with(Bound::Depth, 4)
            .expect("4 is a legal bound");
        assert_eq!(bounds.get(Bound::Depth), 4);
        for (bound, expected) in DEFAULTS {
            if bound != Bound::Depth {
                assert_eq!(bounds.get(bound), expected, "{} changed", bound.name());
            }
        }
    }

    #[test]
    fn parse_limits_new_uses_the_default_schema_bounds() {
        let limits = ParseLimits::new(4096);
        assert_eq!(limits.max_bytes(), 4096);
        assert_eq!(limits.schema(), SchemaBounds::default());
    }

    #[test]
    fn parse_limits_with_schema_keeps_the_byte_bound() {
        let schema = SchemaBounds::default()
            .with(Bound::Routes, 1)
            .expect("1 is a legal bound");
        let limits = ParseLimits::new(64).with_schema(schema);
        assert_eq!(limits.max_bytes(), 64);
        assert_eq!(limits.schema().get(Bound::Routes), 1);
    }
}
