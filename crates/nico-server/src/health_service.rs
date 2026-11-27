//! The upstream-health service: the background loop that turns the probe executor into a cadence.
//!
//! One service, alongside config-reload: it holds the publication handle the reload pipeline
//! writes, reads each generation's member table from it, and hands due members to the probe
//! executor. The process's cancellation token is the only stop signal — a shutdown ends the loop
//! before the drain windows start, so no probe outlives the server that scheduled it.
//!
//! The cadence is the minimum `interval_ms` among the enabled checks, as a fixed tick; the
//! per-member floor (one probe per member per its own interval) is enforced by the members'
//! active state, so a slower check is simply not due on most ticks. The first probe of a member
//! is immediate: a freshly published check does not wait a full interval to speak.
//!
//! The loop is deliberately cheap when nothing is enabled: a generation without any check costs
//! one read of the current snapshot per tick, at a one-second idle tick.

use std::{sync::Arc, time::Duration};

#[allow(unused_imports)]
use async_trait::async_trait;
use nico_http::HttpStateHandle;
use pingora::{server::ShutdownWatch, services::background::BackgroundService};
use tokio_util::sync::CancellationToken;

/// The background health-check service: one instance per process.
pub(crate) struct HealthService {
    /// The publication handle the reload pipeline writes; the loop reads each generation from it.
    state: Arc<HttpStateHandle>,
    /// The process's one stop signal.
    shutdown: CancellationToken,
}

impl HealthService {
    /// Builds the service over the publication handle and the process's stop signal.
    pub(crate) fn new(state: Arc<HttpStateHandle>, shutdown: CancellationToken) -> Self {
        Self { state, shutdown }
    }

    /// Runs until shutdown: tick, hand due members to the probe executor, repeat.
    async fn run_impl(&self) {
        // A short fixed tick; the members' own intervals decide who is actually due.
        let mut tick = tokio::time::interval(Self::tick_period(self.state.current()));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                biased;
                () = self.shutdown.cancelled() => {
                    tracing::debug!("upstream health checks stopping");
                    return;
                }
                _ = tick.tick() => {}
            }
            // One pass over the current generation, whatever it is: a pass over a generation with
            // no checks walks an empty list. Probes run concurrently across members; a member
            // with a probe out skips its turn, so a slow member cannot pile up checks.
            let current = self.state.current();
            let table = current.as_ref().map(|state| state.selection());
            if let Some(table) = table {
                crate::health_service::run_pass(&table).await;
            }
        }
    }

    /// The tick period a generation implies. Enabled or not, the tick is the idle second: the
    /// per-member floor, not the tick, is what a check's `interval_ms` controls.
    fn tick_period(_generation: Option<Arc<nico_http::HttpState>>) -> Duration {
        DEFAULT_TICK
    }
}

/// The idle tick: short enough to notice a new check within a second, long enough that an idle
/// deployment's loop is not a busy poll.
const DEFAULT_TICK: Duration = Duration::from_secs(1);

/// One pass over one generation's members, shared by the loop and the tests.
async fn run_pass(table: &nico_http::SelectionTable) {
    nico_http::run_health_pass(table, std::time::Instant::now()).await;
}

#[async_trait]
impl BackgroundService for HealthService {
    /// The loop Pingora drives; the process's token is the stop signal, the watch is ignored —
    /// the same decision config-reload makes, because there is one shutdown source.
    async fn start(&self, _shutdown: ShutdownWatch) {
        self.run_impl().await;
    }
}
