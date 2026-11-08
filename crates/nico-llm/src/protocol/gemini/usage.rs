//! The `usageMetadata` object an answer reports.
//!
//! This dialect reports the four buckets the canonical model keeps — prompt, candidates, cached
//! content and thoughts — and a count the provider did not report is omitted rather than written as
//! a zero: "the provider said nothing" and "the provider said zero" are different facts. The
//! gateway's own measurement is never written, because a response reports what the provider said.
//! `totalTokenCount` is not written: a total computed here would be a second source of truth beside
//! the buckets it sums.

use serde::Serialize;

use crate::schema::{TokenKind, Usage};

/// The token counts one answer reports.
#[derive(Debug, Clone, Copy, Serialize)]
pub(super) struct UsageWire {
    /// Tokens the request consumed.
    #[serde(rename = "promptTokenCount", skip_serializing_if = "Option::is_none")]
    prompt: Option<u64>,
    /// Tokens the answer produced.
    #[serde(
        rename = "candidatesTokenCount",
        skip_serializing_if = "Option::is_none"
    )]
    candidates: Option<u64>,
    /// Tokens served from the provider's cached content.
    #[serde(
        rename = "cachedContentTokenCount",
        skip_serializing_if = "Option::is_none"
    )]
    cached: Option<u64>,
    /// Tokens spent on hidden reasoning.
    #[serde(rename = "thoughtsTokenCount", skip_serializing_if = "Option::is_none")]
    thoughts: Option<u64>,
}

/// Encodes one canonical usage value, or `None` when nothing was reported at all.
pub(super) fn encode(usage: &Usage) -> Option<UsageWire> {
    let counts = usage.reported_counts()?;
    Some(UsageWire {
        prompt: counts.get(TokenKind::Input),
        candidates: counts.get(TokenKind::Output),
        cached: counts.get(TokenKind::CacheRead),
        thoughts: counts.get(TokenKind::Reasoning),
    })
}
