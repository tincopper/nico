//! Per-route token budget values.

/// The token budget one route's converting requests are bounded by, per authenticated identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenBudget {
    pub(super) limit_tokens: u64,
    pub(super) window_seconds: u32,
}

impl TokenBudget {
    /// Builds a budget from values the parser has already validated.
    pub(super) const fn new(limit_tokens: u64, window_seconds: u32) -> Self {
        Self {
            limit_tokens,
            window_seconds,
        }
    }

    /// Returns the most tokens one identity may spend in one window.
    #[must_use]
    pub const fn limit_tokens(&self) -> u64 {
        self.limit_tokens
    }

    /// Returns how long one budget window lasts, in seconds.
    #[must_use]
    pub const fn window_seconds(&self) -> u32 {
        self.window_seconds
    }
}
