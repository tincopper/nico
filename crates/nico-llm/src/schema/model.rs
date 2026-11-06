//! Model identifiers and the logical-to-physical model pair.
//!
//! A client writes a *logical* model name (`chat-small`), and the route's provider map resolves it
//! to the *physical* name the provider serves (`chat-small-2026-01-01`). Both are [`ModelId`]s: the
//! distinction lives in which accessor of [`ModelRef`] produced the value, not in a second string
//! type, because either side may legitimately address a model and the codec must not guess.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::SchemaError;

/// A non-empty model name.
///
/// The type is the invariant: an empty model selects nothing, so neither a constructor nor serde
/// can produce one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct ModelId(String);

impl ModelId {
    /// Builds a model name.
    ///
    /// # Errors
    /// Returns [`SchemaError::Empty`] when the name is empty or only whitespace.
    pub fn try_new(value: impl Into<String>) -> Result<Self, SchemaError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(SchemaError::Empty { field: "model" });
        }
        Ok(Self(value))
    }

    /// Returns the name exactly as it was written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ModelId {
    type Error = SchemaError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The logical model a client asked for and the physical model the provider will serve.
///
/// The pair is assembled once per request by the provider's `model_map`; neither name is inferred
/// from the other, because a route may map the same logical name differently per provider and a
/// provider may serve one physical model under several aliases.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    logical: ModelId,
    physical: ModelId,
}

impl ModelRef {
    /// Pairs a logical name with the physical name it resolved to.
    #[must_use]
    pub const fn new(logical: ModelId, physical: ModelId) -> Self {
        Self { logical, physical }
    }

    /// Returns the name the client wrote.
    #[must_use]
    pub const fn logical(&self) -> &ModelId {
        &self.logical
    }

    /// Returns the name the provider will receive.
    #[must_use]
    pub const fn physical(&self) -> &ModelId {
        &self.physical
    }
}
