//! The usage document a Chat Completions client reads, built from the provider's reported counts.
//!
//! Only *reported* counts are written. A gateway-measured count is not the provider's bill, and this
//! document is the place a caller reads its bill from: an estimate here would be presented as fact.
//! The two buckets stay separate in the canonical model so this encoder can make that choice
//! explicitly.

use serde::Serialize;

use crate::schema::{TokenKind, Usage};

/// The token counts a response reports.
///
/// Named by the chunk encoder, which never inspects it: the fields stay private so the document has
/// one builder. Not `Copy` — two optional details make it far larger than the tag-sized value the
/// idiom is for, and every use moves it.
#[derive(Debug, Clone, Serialize)]
pub(super) struct UsageWire {
    /// Tokens the caller sent.
    prompt_tokens: u64,
    /// Tokens the model produced.
    completion_tokens: u64,
    /// Their sum.
    total_tokens: u64,
    /// Prompt-side detail, present when the provider reported a cache read.
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_tokens_details: Option<PromptDetails>,
    /// Completion-side detail, present when the provider reported reasoning tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    completion_tokens_details: Option<CompletionDetails>,
}

/// How much of the prompt the provider served from its cache.
#[derive(Debug, Clone, Copy, Serialize)]
struct PromptDetails {
    /// Tokens read from the provider's prompt cache.
    cached_tokens: u64,
}

/// How many output tokens the model spent on hidden reasoning.
#[derive(Debug, Clone, Copy, Serialize)]
struct CompletionDetails {
    /// Reasoning tokens included in the completion count.
    reasoning_tokens: u64,
}

/// Builds the usage document, or `None` when the provider reported no complete pair.
///
/// The document's three totals are required by the protocol, so a partial report — one count, or
/// only gateway-measured counts — produces nothing rather than a document with a made-up member.
pub(super) fn encode(usage: &Usage) -> Option<UsageWire> {
    let counts = usage.reported_counts()?;
    let prompt_tokens = counts.get(TokenKind::Input)?;
    let completion_tokens = counts.get(TokenKind::Output)?;
    let prompt_tokens_details = counts
        .get(TokenKind::CacheRead)
        .map(|cached_tokens| PromptDetails { cached_tokens });
    let completion_tokens_details = counts
        .get(TokenKind::Reasoning)
        .map(|reasoning_tokens| CompletionDetails { reasoning_tokens });
    Some(UsageWire {
        prompt_tokens,
        completion_tokens,
        // A provider could in principle report counts whose sum does not fit, and refusing the whole
        // answer over an accounting detail would be worse than saturating it.
        total_tokens: prompt_tokens.saturating_add(completion_tokens),
        prompt_tokens_details,
        completion_tokens_details,
    })
}
