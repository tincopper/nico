//! The `usage` object an Anthropic Messages caller reads.
//!
//! The protocol always carries one, so a missing reported count is written as `0` rather than
//! omitted: a caller that parses the field would otherwise see a document this protocol never sends.
//! The two cache counts are written only when the provider reported them, because their absence means
//! "this provider does not account for a cache", not "zero tokens were cached".

use serde::Serialize;

use crate::schema::{TokenKind, Usage};

/// The `usage` object of a message or a `message_delta`.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub(super) struct UsageWire {
    /// Input tokens the provider reported, or `0` when it reported none.
    pub(super) input_tokens: u64,
    /// Output tokens the provider reported, or `0` when it reported none.
    pub(super) output_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) cache_creation_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) cache_read_input_tokens: Option<u64>,
}

/// Encodes one canonical usage value.
///
/// A gateway-measured count is never written: a response document reports what the provider said, and
/// an estimate presented as a bill is worse than an absent field.
pub(super) fn encode(usage: &Usage) -> UsageWire {
    let counts = usage.reported_counts();
    UsageWire {
        input_tokens: counts.map_or(0, |counts| counts.get(TokenKind::Input).unwrap_or(0)),
        output_tokens: counts.map_or(0, |counts| counts.get(TokenKind::Output).unwrap_or(0)),
        cache_creation_input_tokens: counts.and_then(|counts| counts.get(TokenKind::CacheWrite)),
        cache_read_input_tokens: counts.and_then(|counts| counts.get(TokenKind::CacheRead)),
    }
}
