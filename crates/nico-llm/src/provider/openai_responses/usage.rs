//! The usage document, decoded into the canonical reported counts.
//!
//! Only the provider's *reported* counts are read into [`Usage::reported`]; the gateway's own
//! measurement is added elsewhere. `total_tokens` is the sum of the two counts the canonical model
//! already stores, so it is not carried a second time.
//!
//! This dialect spells the same facts differently from Chat Completions (`input_tokens` rather than
//! `prompt_tokens`), so the shape is its own. Sharing one struct would mean one adapter decoding a
//! field the other's wire never sends.
//!
//! Unknown members are read and dropped rather than rejected. A usage object is accounting detail,
//! not request semantics: an audio-token extension cannot change the answer the caller receives, and
//! failing an otherwise complete response over an accounting extra would make the gateway refuse a
//! response it can serve. The one place that would be a lie — presenting a count the provider did not
//! report — is the reason only `Some` values become counts.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::schema::{RawJson, TokenKind, Usage, UsageCounts};

/// The token counts a response reports.
#[derive(Debug, Deserialize)]
pub(super) struct UsageWire {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    input_tokens_details: Option<InputDetails>,
    #[serde(default)]
    output_tokens_details: Option<OutputDetails>,
    #[serde(flatten)]
    _extra: BTreeMap<String, RawJson>,
}

/// Prompt-side detail, of which only the cache read has a canonical count.
#[derive(Debug, Deserialize)]
struct InputDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
    #[serde(flatten)]
    _extra: BTreeMap<String, RawJson>,
}

/// Completion-side detail, of which only reasoning tokens have a canonical count.
#[derive(Debug, Deserialize)]
struct OutputDetails {
    #[serde(default)]
    reasoning_tokens: Option<u64>,
    #[serde(flatten)]
    _extra: BTreeMap<String, RawJson>,
}

/// Reads the counts the provider reported.
///
/// A document with no count at all yields an empty [`Usage`] rather than a reported bucket of zeros:
/// "the provider said nothing" and "the provider said zero" are different facts.
pub(super) fn decode(wire: &UsageWire) -> Usage {
    let mut counts = UsageCounts::new();
    if let Some(value) = wire.input_tokens {
        counts.set(TokenKind::Input, value);
    }
    if let Some(value) = wire.output_tokens {
        counts.set(TokenKind::Output, value);
    }
    if let Some(value) = wire
        .input_tokens_details
        .as_ref()
        .and_then(|details| details.cached_tokens)
    {
        counts.set(TokenKind::CacheRead, value);
    }
    if let Some(value) = wire
        .output_tokens_details
        .as_ref()
        .and_then(|details| details.reasoning_tokens)
    {
        counts.set(TokenKind::Reasoning, value);
    }
    if counts.is_empty() {
        Usage::default()
    } else {
        Usage::reported(counts)
    }
}
