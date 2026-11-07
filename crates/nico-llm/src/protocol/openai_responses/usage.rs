//! The `usage` object a Responses caller reads.
//!
//! This protocol names the token counts itself (`input_tokens`/`output_tokens`) rather than reusing the
//! chat dialect's `prompt`/`completion` spelling, and it nests the cache and reasoning counts. A count
//! the provider did not report is omitted, and a gateway estimate is never written: a response reports
//! what the provider said.

use serde::Serialize;

use crate::schema::{TokenKind, Usage};

/// The `usage` object of a response.
#[derive(Debug, Clone, Copy, Serialize)]
pub(super) struct UsageWire {
    /// Tokens sent to the provider.
    pub(super) input_tokens: u64,
    /// Tokens the model produced.
    pub(super) output_tokens: u64,
    /// The sum of the two, which this protocol always carries.
    pub(super) total_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) input_tokens_details: Option<InputDetails>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) output_tokens_details: Option<OutputDetails>,
}

/// The input-side breakdown.
#[derive(Debug, Clone, Copy, Serialize)]
pub(super) struct InputDetails {
    /// Tokens served from the provider's prompt cache.
    pub(super) cached_tokens: u64,
}

/// The output-side breakdown.
#[derive(Debug, Clone, Copy, Serialize)]
pub(super) struct OutputDetails {
    /// Tokens spent on hidden reasoning.
    pub(super) reasoning_tokens: u64,
}

/// Encodes one canonical usage value, or `None` when the provider reported no counts at all.
pub(super) fn encode(usage: &Usage) -> Option<UsageWire> {
    let counts = usage.reported_counts()?;
    let input = counts.get(TokenKind::Input).unwrap_or(0);
    let output = counts.get(TokenKind::Output).unwrap_or(0);
    Some(UsageWire {
        input_tokens: input,
        output_tokens: output,
        total_tokens: input.saturating_add(output),
        input_tokens_details: counts
            .get(TokenKind::CacheRead)
            .map(|cached_tokens| InputDetails { cached_tokens }),
        output_tokens_details: counts
            .get(TokenKind::Reasoning)
            .map(|reasoning_tokens| OutputDetails { reasoning_tokens }),
    })
}
