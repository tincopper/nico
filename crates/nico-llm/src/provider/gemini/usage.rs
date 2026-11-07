//! The usage document, decoded into the canonical reported counts.
//!
//! Only the provider's *reported* counts are read into [`Usage::reported`]; the gateway's own
//! measurement is added elsewhere. This dialect reports `promptTokenCount` (which already includes
//! `cachedContentTokenCount`), `candidatesTokenCount`, and `thoughtsTokenCount` for output spent on
//! hidden reasoning, which is why the canonical model keeps four buckets rather than folding one into
//! another. `totalTokenCount` is not carried: a total the protocol computed from its own buckets would
//! become a second, disagreeing source of truth once another protocol's counts are decoded.
//!
//! Unknown members are read and dropped rather than rejected. A usage object is accounting detail,
//! not request semantics: an accounting extra cannot change the answer the caller receives, and
//! failing an otherwise complete response over one would make the gateway refuse a response it can
//! serve. The one place that would be a lie — presenting a count the provider did not report — is the
//! reason only `Some` values become counts.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::schema::{RawJson, TokenKind, Usage, UsageCounts};

/// The token counts one answer reports.
#[derive(Debug, Default, Deserialize)]
pub(super) struct UsageWire {
    #[serde(default, rename = "promptTokenCount")]
    prompt: Option<u64>,
    #[serde(default, rename = "candidatesTokenCount")]
    candidates: Option<u64>,
    #[serde(default, rename = "cachedContentTokenCount")]
    cached: Option<u64>,
    #[serde(default, rename = "thoughtsTokenCount")]
    thoughts: Option<u64>,
    #[serde(default, rename = "totalTokenCount")]
    _total: Option<u64>,
    #[serde(flatten)]
    _extra: BTreeMap<String, RawJson>,
}

/// Reads the counts the provider reported.
///
/// A document with no count at all yields an empty [`Usage`] rather than a reported bucket of zeros:
/// "the provider said nothing" and "the provider said zero" are different facts.
pub(super) fn decode(wire: &UsageWire) -> Usage {
    let mut counts = UsageCounts::new();
    if let Some(value) = wire.prompt {
        counts.set(TokenKind::Input, value);
    }
    if let Some(value) = wire.candidates {
        counts.set(TokenKind::Output, value);
    }
    if let Some(value) = wire.cached {
        counts.set(TokenKind::CacheRead, value);
    }
    if let Some(value) = wire.thoughts {
        counts.set(TokenKind::Reasoning, value);
    }
    if counts.is_empty() {
        Usage::default()
    } else {
        Usage::reported(counts)
    }
}
