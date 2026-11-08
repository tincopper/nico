//! The matrix table itself: sixteen rows and four native pairs, written out and never derived.
//!
//! A registry that contains four codecs and four adapters does not say they may be paired, and a
//! pair that is missing a half must be a test failure rather than a silently skipped combination.
//! The capability sets are the design's §6 table: a row's set is the intersection of the two sides'
//! declarations, and a side that declares a capability it does not map would fail the row here.

use nico_llm::{
    Capabilities, Capability, ProtocolId, ProviderKind,
    protocol::{
        anthropic::PROTOCOL as ANTHROPIC_MESSAGES, gemini::PROTOCOL as GEMINI_GENERATE_CONTENT,
        openai_chat::PROTOCOL as OPENAI_CHAT, openai_responses::PROTOCOL as OPENAI_RESPONSES,
    },
    provider::{
        anthropic::PROVIDER_KIND as ANTHROPIC, gemini::PROVIDER_KIND as GEMINI,
        openai::PROVIDER_KIND as OPENAI,
        openai_responses::PROVIDER_KIND as OPENAI_RESPONSES_ADAPTER,
    },
};

/// One row of the matrix: a `(client protocol, provider kind)` pair and the capability set the pair
/// serves — the intersection of the two sides' declarations.
pub(crate) struct Row {
    /// The pair, named for the failure message.
    pub(crate) name: &'static str,
    /// The client protocol the row's requests speak.
    pub(crate) protocol: ProtocolId,
    /// The provider kind the row's requests reach.
    pub(crate) kind: ProviderKind,
    /// The capability set the compiled exchange must report.
    pub(crate) capabilities: Capabilities,
}

/// Builds a capability set from the five axes, in the design's table order.
const fn caps(
    streaming: bool,
    tools: bool,
    images: bool,
    reasoning: bool,
    format: bool,
) -> Capabilities {
    let capabilities = Capabilities::none();
    let capabilities = if streaming {
        capabilities.with(Capability::Streaming)
    } else {
        capabilities
    };
    let capabilities = if tools {
        capabilities.with(Capability::Tools)
    } else {
        capabilities
    };
    let capabilities = if images {
        capabilities.with(Capability::Images)
    } else {
        capabilities
    };
    let capabilities = if reasoning {
        capabilities.with(Capability::Reasoning)
    } else {
        capabilities
    };
    if format {
        capabilities.with(Capability::ResponseFormat)
    } else {
        capabilities
    }
}

/// Every capability.
pub(crate) const ALL: Capabilities = caps(true, true, true, true, true);

/// The four axes minus reasoning: a dialect whose replay needs handles the canonical model lacks.
pub(crate) const NO_REASONING: Capabilities = caps(true, true, true, false, true);

/// The three axes both OpenAI adapters share with the Anthropic one.
pub(crate) const NO_REASONING_NO_FORMAT: Capabilities = caps(true, true, true, false, false);

/// The four axes minus the structured format: the Anthropic codec's own table row.
pub(crate) const NO_FORMAT: Capabilities = caps(true, true, true, true, false);

/// The sixteen rows: every codec against every adapter.
///
/// The table is explicit on purpose: adding a codec or an adapter without adding its rows fails the
/// `len` assertion in [`crate::the_whole_matrix_is_served`], and a row whose capability set drifts
/// from what the two sides really map fails that row.
pub(crate) const MATRIX: &[Row] = &[
    Row {
        name: "chat × openai",
        protocol: OPENAI_CHAT,
        kind: OPENAI,
        capabilities: ALL,
    },
    Row {
        name: "chat × openai_responses",
        protocol: OPENAI_CHAT,
        kind: OPENAI_RESPONSES_ADAPTER,
        capabilities: NO_REASONING,
    },
    Row {
        name: "chat × anthropic",
        protocol: OPENAI_CHAT,
        kind: ANTHROPIC,
        capabilities: NO_REASONING_NO_FORMAT,
    },
    Row {
        name: "chat × gemini",
        protocol: OPENAI_CHAT,
        kind: GEMINI,
        capabilities: ALL,
    },
    Row {
        name: "responses × openai",
        protocol: OPENAI_RESPONSES,
        kind: OPENAI,
        capabilities: NO_REASONING,
    },
    Row {
        name: "responses × openai_responses",
        protocol: OPENAI_RESPONSES,
        kind: OPENAI_RESPONSES_ADAPTER,
        capabilities: NO_REASONING,
    },
    Row {
        name: "responses × anthropic",
        protocol: OPENAI_RESPONSES,
        kind: ANTHROPIC,
        capabilities: NO_REASONING_NO_FORMAT,
    },
    Row {
        name: "responses × gemini",
        protocol: OPENAI_RESPONSES,
        kind: GEMINI,
        capabilities: NO_REASONING,
    },
    Row {
        name: "messages × openai",
        protocol: ANTHROPIC_MESSAGES,
        kind: OPENAI,
        capabilities: NO_FORMAT,
    },
    Row {
        name: "messages × openai_responses",
        protocol: ANTHROPIC_MESSAGES,
        kind: OPENAI_RESPONSES_ADAPTER,
        capabilities: NO_REASONING_NO_FORMAT,
    },
    Row {
        name: "messages × anthropic",
        protocol: ANTHROPIC_MESSAGES,
        kind: ANTHROPIC,
        capabilities: NO_REASONING_NO_FORMAT,
    },
    Row {
        name: "messages × gemini",
        protocol: ANTHROPIC_MESSAGES,
        kind: GEMINI,
        capabilities: NO_FORMAT,
    },
    Row {
        name: "generate_content × openai",
        protocol: GEMINI_GENERATE_CONTENT,
        kind: OPENAI,
        capabilities: ALL,
    },
    Row {
        name: "generate_content × openai_responses",
        protocol: GEMINI_GENERATE_CONTENT,
        kind: OPENAI_RESPONSES_ADAPTER,
        capabilities: NO_REASONING,
    },
    Row {
        name: "generate_content × anthropic",
        protocol: GEMINI_GENERATE_CONTENT,
        kind: ANTHROPIC,
        capabilities: NO_REASONING_NO_FORMAT,
    },
    Row {
        name: "generate_content × gemini",
        protocol: GEMINI_GENERATE_CONTENT,
        kind: GEMINI,
        capabilities: ALL,
    },
];

/// One native pair: the `(protocol, kind)` rows where both sides name the same wire dialect.
///
/// A `passthrough` route is legal exactly when the pair is native, and even then the route must be
/// publishable: three of the four are, and this dialect's adapter refuses the fourth outright — its
/// request target carries the model and the operation, so forwarding fixed bytes would answer a
/// different question than the one the caller asked.
pub(crate) struct NativePair {
    /// The pair, named for the failure message.
    pub(crate) name: &'static str,
    /// The client protocol, which must be the adapter's native one.
    pub(crate) protocol: ProtocolId,
    /// The provider kind, whose native protocol must be the same value.
    pub(crate) kind: ProviderKind,
    /// Whether the pair's passthrough route publishes.
    pub(crate) publishable: bool,
}

/// The four native pairs a passthrough route can name.
pub(crate) const NATIVE: &[NativePair] = &[
    NativePair {
        name: "chat × openai",
        protocol: OPENAI_CHAT,
        kind: OPENAI,
        publishable: true,
    },
    NativePair {
        name: "responses × openai_responses",
        protocol: OPENAI_RESPONSES,
        kind: OPENAI_RESPONSES_ADAPTER,
        publishable: true,
    },
    NativePair {
        name: "messages × anthropic",
        protocol: ANTHROPIC_MESSAGES,
        kind: ANTHROPIC,
        publishable: true,
    },
    NativePair {
        name: "generate_content × gemini",
        protocol: GEMINI_GENERATE_CONTENT,
        kind: GEMINI,
        publishable: false,
    },
];
