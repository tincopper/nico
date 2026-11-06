//! Sampling knobs shared by every target protocol.
//!
//! The knob *set* is the common denominator: a knob named here can be expressed by every protocol a
//! route can reach, so a range failure is reported once, at construction, instead of surfacing as a
//! provider error after the request has been forwarded. A knob's range is the widest one a target
//! protocol accepts, never the intersection: a value the client's own protocol allows is not refused
//! by the canonical model, and a protocol whose own range is narrower refuses in its codec — the
//! place that reads the wire document — instead of clamping a value the caller asked for.
//!
//! The floats are `f64` because that is what a JSON number is: storing `f32` would make a canonical
//! round trip lossy (`0.2` would serialize back as `0.20000000298023224`) and a golden fixture could
//! not compare equal. A sampling knob the canonical model does not name is rejected rather than
//! dropped: `Sampling` has no extension bucket, and silently forwarding an unknown knob would let a
//! caller believe it took effect.

use serde::{Deserialize, Serialize};

use super::SchemaError;

/// Highest `temperature` accepted; the widest range a target protocol allows.
const MAX_TEMPERATURE: f64 = 2.0;
/// Lowest value accepted for a ratio knob (`temperature`, `top_p`).
const MIN_RATIO: f64 = 0.0;
/// Highest `top_p` accepted.
const MAX_TOP_P: f64 = 1.0;
/// Lowest `presence_penalty`/`frequency_penalty` accepted; the range of the protocols that carry them.
const MIN_PENALTY: f64 = -2.0;
/// Highest `presence_penalty`/`frequency_penalty` accepted.
const MAX_PENALTY: f64 = 2.0;
/// Most stop sequences one request may carry; the narrowest documented limit of the target protocols.
const MAX_STOP_SEQUENCES: usize = 4;

/// Sampling knobs, all optional so "unset" stays distinct from "explicitly zero".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "SamplingWire")]
pub struct Sampling {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    stop: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    seed: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    presence_penalty: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    frequency_penalty: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SamplingWire {
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    top_p: Option<f64>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    stop: Vec<String>,
    #[serde(default)]
    seed: Option<u64>,
    #[serde(default)]
    presence_penalty: Option<f64>,
    #[serde(default)]
    frequency_penalty: Option<f64>,
}

impl Sampling {
    /// Builds the three knobs every protocol has carried since its first version.
    ///
    /// The remaining knobs are set through their `with_*` methods, which is also why this
    /// constructor keeps its shape: a caller that only needs the common three does not have to name
    /// the rest.
    ///
    /// # Errors
    /// Returns [`SchemaError::NotFinite`] or [`SchemaError::OutOfRange`] for a non-finite or
    /// out-of-range `temperature`/`top_p`, and [`SchemaError::OutOfRange`] for `max_tokens == 0`.
    pub fn try_new(
        temperature: Option<f64>,
        top_p: Option<f64>,
        max_tokens: Option<u32>,
    ) -> Result<Self, SchemaError> {
        let sampling = Self {
            temperature,
            top_p,
            max_tokens,
            ..Self::default()
        };
        sampling.validate()?;
        Ok(sampling)
    }

    /// Sets `temperature`, which controls randomness.
    ///
    /// # Errors
    /// Returns [`SchemaError::NotFinite`] or [`SchemaError::OutOfRange`] when the value is not in
    /// `0.0..=2.0`.
    pub fn with_temperature(mut self, temperature: f64) -> Result<Self, SchemaError> {
        validate_unit_interval("sampling.temperature", temperature, MAX_TEMPERATURE)?;
        self.temperature = Some(temperature);
        Ok(self)
    }

    /// Sets `top_p`, which restricts sampling to the most likely tokens.
    ///
    /// # Errors
    /// Returns [`SchemaError::NotFinite`] or [`SchemaError::OutOfRange`] when the value is not in
    /// `0.0..=1.0`.
    pub fn with_top_p(mut self, top_p: f64) -> Result<Self, SchemaError> {
        validate_unit_interval("sampling.top_p", top_p, MAX_TOP_P)?;
        self.top_p = Some(top_p);
        Ok(self)
    }

    /// Sets the output token ceiling.
    ///
    /// # Errors
    /// Returns [`SchemaError::OutOfRange`] when `max_tokens` is zero.
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Result<Self, SchemaError> {
        validate_max_tokens(max_tokens)?;
        self.max_tokens = Some(max_tokens);
        Ok(self)
    }

    /// Replaces the sequences that make the model stop generating.
    ///
    /// # Errors
    /// Returns [`SchemaError::OutOfRange`] for more than four sequences, and [`SchemaError::Empty`]
    /// for an empty sequence. Whitespace is a legitimate stop sequence (`"\n\n"` ends a paragraph),
    /// so only a zero-length one is refused.
    pub fn with_stop(mut self, stop: Vec<String>) -> Result<Self, SchemaError> {
        validate_stop(&stop)?;
        self.stop = stop;
        Ok(self)
    }

    /// Sets the seed a provider may use to make sampling reproducible.
    #[must_use]
    pub fn with_seed(mut self, seed: u64) -> Self {
        self.seed = Some(seed);
        self
    }

    /// Sets how much the model is penalized for repeating a token it has already produced.
    ///
    /// # Errors
    /// Returns [`SchemaError::NotFinite`] or [`SchemaError::OutOfRange`] when the value is not in
    /// `-2.0..=2.0`.
    pub fn with_presence_penalty(mut self, penalty: f64) -> Result<Self, SchemaError> {
        validate_penalty("sampling.presence_penalty", penalty)?;
        self.presence_penalty = Some(penalty);
        Ok(self)
    }

    /// Sets how much the model is penalized in proportion to how often a token has appeared.
    ///
    /// # Errors
    /// Returns [`SchemaError::NotFinite`] or [`SchemaError::OutOfRange`] when the value is not in
    /// `-2.0..=2.0`.
    pub fn with_frequency_penalty(mut self, penalty: f64) -> Result<Self, SchemaError> {
        validate_penalty("sampling.frequency_penalty", penalty)?;
        self.frequency_penalty = Some(penalty);
        Ok(self)
    }

    /// Checks every set knob.
    ///
    /// # Errors
    /// Returns the same variants as the individual setters.
    pub fn validate(&self) -> Result<(), SchemaError> {
        if let Some(temperature) = self.temperature {
            validate_unit_interval("sampling.temperature", temperature, MAX_TEMPERATURE)?;
        }
        if let Some(top_p) = self.top_p {
            validate_unit_interval("sampling.top_p", top_p, MAX_TOP_P)?;
        }
        if let Some(max_tokens) = self.max_tokens {
            validate_max_tokens(max_tokens)?;
        }
        validate_stop(&self.stop)?;
        if let Some(penalty) = self.presence_penalty {
            validate_penalty("sampling.presence_penalty", penalty)?;
        }
        if let Some(penalty) = self.frequency_penalty {
            validate_penalty("sampling.frequency_penalty", penalty)?;
        }
        Ok(())
    }

    /// Returns `temperature`.
    #[must_use]
    pub const fn temperature(&self) -> Option<f64> {
        self.temperature
    }

    /// Returns `top_p`.
    #[must_use]
    pub const fn top_p(&self) -> Option<f64> {
        self.top_p
    }

    /// Returns the output token ceiling.
    #[must_use]
    pub const fn max_tokens(&self) -> Option<u32> {
        self.max_tokens
    }

    /// Returns the stop sequences, empty when the caller set none.
    #[must_use]
    pub fn stop(&self) -> &[String] {
        &self.stop
    }

    /// Returns the reproducibility seed, when the caller set one.
    #[must_use]
    pub const fn seed(&self) -> Option<u64> {
        self.seed
    }

    /// Returns the presence penalty.
    #[must_use]
    pub const fn presence_penalty(&self) -> Option<f64> {
        self.presence_penalty
    }

    /// Returns the frequency penalty.
    #[must_use]
    pub const fn frequency_penalty(&self) -> Option<f64> {
        self.frequency_penalty
    }
}

impl TryFrom<SamplingWire> for Sampling {
    type Error = SchemaError;

    fn try_from(wire: SamplingWire) -> Result<Self, Self::Error> {
        let sampling = Self {
            temperature: wire.temperature,
            top_p: wire.top_p,
            max_tokens: wire.max_tokens,
            stop: wire.stop,
            seed: wire.seed,
            presence_penalty: wire.presence_penalty,
            frequency_penalty: wire.frequency_penalty,
        };
        sampling.validate()?;
        Ok(sampling)
    }
}

fn validate_unit_interval(
    field: &'static str,
    value: f64,
    maximum: f64,
) -> Result<(), SchemaError> {
    if !value.is_finite() {
        return Err(SchemaError::NotFinite { field });
    }
    if !(MIN_RATIO..=maximum).contains(&value) {
        return Err(SchemaError::OutOfRange {
            field,
            value: value.to_string(),
        });
    }
    Ok(())
}

fn validate_penalty(field: &'static str, value: f64) -> Result<(), SchemaError> {
    if !value.is_finite() {
        return Err(SchemaError::NotFinite { field });
    }
    if !(MIN_PENALTY..=MAX_PENALTY).contains(&value) {
        return Err(SchemaError::OutOfRange {
            field,
            value: value.to_string(),
        });
    }
    Ok(())
}

fn validate_max_tokens(max_tokens: u32) -> Result<(), SchemaError> {
    if max_tokens == 0 {
        return Err(SchemaError::OutOfRange {
            field: "sampling.max_tokens",
            value: max_tokens.to_string(),
        });
    }
    Ok(())
}

fn validate_stop(stop: &[String]) -> Result<(), SchemaError> {
    if stop.len() > MAX_STOP_SEQUENCES {
        return Err(SchemaError::OutOfRange {
            field: "sampling.stop",
            value: format!("{} sequences", stop.len()),
        });
    }
    if stop.iter().any(String::is_empty) {
        return Err(SchemaError::Empty {
            field: "sampling.stop",
        });
    }
    Ok(())
}
