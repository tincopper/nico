//! The state one deployment shares across configuration generations.
//!
//! A publication replaces the snapshot, the plugin chains and the LLM plan; the things a target
//! **learned** while serving — whether its breaker opened — must not be replaced with it. This is
//! the process-level holder for that state: created once while the process is assembled, shared by
//! every generation through an `Arc`, and never rebuilt by a reload.
//!
//! The minimal form this milestone ships is the breaker table. It is deliberately not a generic
//! lifecycle manager: a second kind of long-lived state arrives with its own field here, with the
//! rule that whatever lives in it survives a reload only while the document still names it.

use std::sync::Arc;

use nico_llm::{BreakerTable, route::ExclusionTable};

/// The process-level state that outlives every configuration generation.
pub struct SharedServices {
    breakers: Arc<BreakerTable>,
    /// Members a pool retired for a category-chosen cooldown.
    ///
    /// The second piece of learned state, and it lives beside the breaker for the same reason: it is
    /// earned by attempts, so a reload must not reset it. It is a *separate* table on purpose — a
    /// `429` retires a member for ten seconds, while the breaker counts service failures and opens for
    /// the document's cooldown. Merging them would let one rate limit look like an outage.
    exclusions: Arc<ExclusionTable>,
}

impl SharedServices {
    /// Builds the process's shared services: no target has state yet.
    ///
    /// A breaker's history is earned by attempts, not declared by configuration, so a process
    /// starts with no opinion about any endpoint.
    #[must_use]
    pub fn new() -> Self {
        Self {
            breakers: Arc::new(BreakerTable::new()),
            exclusions: Arc::new(ExclusionTable::new()),
        }
    }

    /// Returns the breaker state every generation shares.
    ///
    /// The `Arc` is the contract: a publication compiles chains against this table, the request path
    /// consults and updates it, and nothing in the reload pipeline may replace it.
    #[must_use]
    pub fn breakers(&self) -> &Arc<BreakerTable> {
        &self.breakers
    }

    /// Returns the exclusion table every generation shares.
    ///
    /// Same contract as [`SharedServices::breakers`]: publication compiles against it, the request
    /// path reads and writes it, and a reload never replaces it.
    #[must_use]
    pub fn exclusions(&self) -> &Arc<ExclusionTable> {
        &self.exclusions
    }
}

impl Default for SharedServices {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for SharedServices {
    /// Reports what the services hold, through the table's own `Debug`: a target count and nothing
    /// that names an endpoint or a credential.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SharedServices")
            .field("breakers", &self.breakers)
            .field("exclusions", &self.exclusions)
            .finish()
    }
}

// Compiled-in proof that the process's one shared state can be held by every task that compiles or
// serves a generation.
const _: fn() = || {
    const fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<SharedServices>();
};

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use nico_llm::{AttemptOutcome, BreakerParams, BreakerPhase, provider::ProviderKind};
    use tokio::time::Instant;

    use super::*;

    fn identity(provider: &str, threshold: u32) -> nico_llm::TargetIdentity {
        nico_llm::TargetIdentity::new(
            provider,
            "http://127.0.0.1:8080",
            ProviderKind::new("mock"),
            BreakerParams::new(threshold, 30_000),
        )
    }

    #[test]
    fn one_instance_serves_every_handle_the_process_holds() {
        // The lifecycle contract: one SharedServices per process, every generation reading and
        // writing the same table through its own handle.
        let shared = Arc::new(SharedServices::new());
        let reader = Arc::clone(&shared);
        let target = identity("primary", 1);

        shared
            .breakers()
            .record(&target, AttemptOutcome::Failure, None, Instant::now());
        let admission = reader
            .breakers()
            .admit(&target, Instant::now() + Duration::from_secs(1));
        assert!(
            matches!(admission, nico_llm::Admission::Skip(BreakerPhase::Open)),
            "state written through one handle is read through the other"
        );
    }

    #[test]
    fn a_generation_that_drops_a_target_drops_its_state() {
        let shared = SharedServices::new();
        let target = identity("primary", 1);
        let now = Instant::now();
        shared
            .breakers()
            .record(&target, AttemptOutcome::Failure, None, now);

        // The next generation's compile collected its live identities and pruned the table.
        shared
            .breakers()
            .retain(|live| live.provider() != "primary");
        let admission = shared.breakers().admit(&target, now);
        assert!(
            matches!(admission, nico_llm::Admission::Admit),
            "a target the document no longer serves has no history to skip on"
        );
    }
}
