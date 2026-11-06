//! The canonical model's two untyped escape hatches: [`RawJson`] and [`Extensions`].
//!
//! The backend spec forbids `serde_json::Value` on the request path because untyped lookups lose
//! the shape, the validation and the compiler's help. There are exactly two classes of value for
//! which the gateway genuinely cannot know the shape:
//!
//! 1. **Caller-supplied JSON documents.** A tool's parameter schema, a tool call's arguments and a
//!    structured-output schema are declared by the caller, so the gateway can check that they are
//!    valid JSON with the right top-level kind, but not which keys they hold. They are sent to the
//!    provider verbatim.
//! 2. **Unmodelled fields.** A provider or client may send a field this model does not name. The
//!    spec requires that such a field is preserved rather than silently dropped, so it is carried
//!    in [`Extensions`] as [`RawJson`].
//!
//! [`RawJson`] is a deliberate newtype rather than a bare `Value`: a `grep RawJson` lists every
//! place the untyped exception is used, and configuration, routing and plugin data keep their no
//! `Value` rule. Nothing else in `schema/` stores a `Value` directly.
//!
//! [`Extensions`] sits on [`ChatRequest`](super::ChatRequest) and
//! [`ChatResponse`](super::ChatResponse), so it captures fields at that level. A *nested* field the
//! shape does not model (an unknown key inside a message, for example) is the codec's decision: it
//! lifts the field into the enclosing extensions under a dotted key or rejects the request as
//! unsupported, but it never drops it. The canonical serde form reflects that split — it captures a
//! top-level unknown and ignores a nested one — because only the codec knows the wire spelling a
//! nested field arrived in.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::SchemaError;

/// A JSON value whose shape the canonical model does not type.
///
/// Use it for a caller-declared JSON document — a tool's parameters, a tool call's arguments, a
/// structured-output schema — and for unmodelled fields inside [`Extensions`]; nothing else. The
/// type carries no shape invariant of its own: callers that need an object (a tool's parameters, a
/// call's arguments) check it, because a schema or an arguments document that is not an object
/// cannot be sent to any provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RawJson(Value);

impl RawJson {
    /// Wraps an already-parsed JSON value.
    #[must_use]
    pub fn new(value: Value) -> Self {
        Self(value)
    }

    /// Parses JSON text.
    ///
    /// Some client protocols carry tool arguments as a JSON *string*, so a codec needs one place
    /// that turns text into the untyped value while reporting a typed failure.
    ///
    /// # Errors
    /// Returns [`SchemaError::InvalidJson`] when the text is not valid JSON.
    pub fn parse(text: &str) -> Result<Self, SchemaError> {
        serde_json::from_str(text)
            .map(Self)
            .map_err(|error| SchemaError::InvalidJson {
                field: "raw_json",
                reason: error.to_string(),
            })
    }

    /// Returns the wrapped value.
    #[must_use]
    pub const fn as_value(&self) -> &Value {
        &self.0
    }

    /// Consumes the wrapper and returns the value.
    #[must_use]
    pub fn into_value(self) -> Value {
        self.0
    }

    /// Reports whether the value is a JSON object.
    #[must_use]
    pub fn is_object(&self) -> bool {
        self.0.is_object()
    }

    /// Reports whether the value is an object with no members.
    #[must_use]
    pub fn is_empty_object(&self) -> bool {
        self.0.as_object().is_some_and(serde_json::Map::is_empty)
    }

    /// Renders the value as compact JSON text.
    #[must_use]
    pub fn to_text(&self) -> String {
        self.0.to_string()
    }
}

impl From<Value> for RawJson {
    fn from(value: Value) -> Self {
        Self(value)
    }
}

/// Fields a client or provider sent that the canonical model does not name.
///
/// The map is deliberately a `BTreeMap`: extensions are serialized back to a client and compared in
/// fixtures, so a stable order keeps both deterministic. An empty key is rejected — it cannot be
/// serialized back into a meaningful JSON object — and keys are otherwise passed through verbatim.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Extensions(BTreeMap<String, RawJson>);

impl Extensions {
    /// Builds an empty set of extensions.
    #[must_use]
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Inserts one unmodelled field.
    ///
    /// # Errors
    /// Returns [`SchemaError::Empty`] when the key is empty.
    pub fn try_insert(
        &mut self,
        key: impl Into<String>,
        value: impl Into<RawJson>,
    ) -> Result<(), SchemaError> {
        let key = key.into();
        if key.is_empty() {
            return Err(SchemaError::Empty {
                field: "extensions key",
            });
        }
        self.0.insert(key, value.into());
        Ok(())
    }

    /// Returns the value stored under `key`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&RawJson> {
        self.0.get(key)
    }

    /// Iterates the unmodelled fields in key order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &RawJson)> {
        self.0.iter().map(|(key, value)| (key.as_str(), value))
    }

    /// Returns how many unmodelled fields are stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Reports whether no unmodelled field is stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Checks every key.
    ///
    /// # Errors
    /// Returns [`SchemaError::Empty`] when a key is empty.
    pub fn validate(&self) -> Result<(), SchemaError> {
        if self.0.keys().any(String::is_empty) {
            return Err(SchemaError::Empty {
                field: "extensions key",
            });
        }
        Ok(())
    }

    /// Rejects an extension key that duplicates a field the enclosing shape already models.
    ///
    /// Extensions are serialized next to the modelled fields. A key that shadows one would be
    /// overwritten on the way out or swallowed on the way in, so the enclosing `validate` checks
    /// this instead of letting a round trip silently change the value.
    ///
    /// # Errors
    /// Returns [`SchemaError::ReservedExtensionKey`] naming the first shadowing key.
    pub fn reject_modelled_keys(&self, modelled: &[&'static str]) -> Result<(), SchemaError> {
        for (key, _) in self.iter() {
            if modelled.contains(&key) {
                return Err(SchemaError::ReservedExtensionKey {
                    key: key.to_owned(),
                });
            }
        }
        Ok(())
    }
}
