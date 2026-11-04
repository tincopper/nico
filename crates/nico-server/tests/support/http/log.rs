//! Captures the data plane's `tracing` output so a test can assert what an operator would read.
//!
//! Two acceptance criteria are statements about the log, not about the wire: a client disconnect
//! must be recorded as `Cancelled` (never as a 5xx), and a request must carry `route_id`,
//! `upstream`, `config.generation`, `http.status` and `elapsed_ms`. The request log is a `tracing`
//! event on whatever thread Pingora drives, so the only way to assert it is to install one
//! subscriber for the process and read back what it wrote.
//!
//! The subscriber is installed once per test binary; a test binary runs one server (see the parent
//! module), so a second installation would be a bug rather than a race.

use std::{
    io::{self, Write},
    sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError},
    time::{Duration, Instant},
};

use tracing::Level;

/// Process-wide sink for the formatted log lines.
static CAPTURE: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();

/// Installs the capturing subscriber, or returns the buffer a previous call installed.
pub(crate) fn capture() -> Arc<Mutex<Vec<u8>>> {
    let buffer = CAPTURE.get_or_init(|| {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        // The data plane logs a normal request at `debug!`, so the level has to be at least that
        // low or the record the test is looking for would be filtered out by the subscriber.
        let _installed = tracing_subscriber::fmt()
            .with_writer(Sink(Arc::clone(&buffer)))
            .with_ansi(false)
            .with_target(false)
            .with_max_level(Level::DEBUG)
            .try_init();
        buffer
    });
    Arc::clone(buffer)
}

/// Returns everything captured so far as text.
pub(crate) fn text(buffer: &Arc<Mutex<Vec<u8>>>) -> String {
    let bytes = lock(buffer).clone();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Waits until a captured line satisfies `matches`, and returns it.
///
/// The log record is written while the request is being finished, after any wire observation the
/// test already waited for, and there is no notification to await — so the wait polls the buffer
/// with a bounded budget instead of guessing a sleep.
pub(crate) async fn wait_for_line_matching(
    buffer: &Arc<Mutex<Vec<u8>>>,
    budget: Duration,
    matches: impl Fn(&str) -> bool,
) -> String {
    let deadline = Instant::now() + budget;
    loop {
        let captured = text(buffer);
        if let Some(line) = captured.lines().find(|line| matches(line)) {
            return line.to_owned();
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "no matching log line within {budget:?}; captured:\n{captured}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Waits for the data plane's own request record for `trace_id`.
///
/// Pingora also logs about a request under the same correlation id, so the record is identified by
/// a field only the request log carries (`http.status`), not by the id alone.
pub(crate) async fn wait_for_request_log(
    buffer: &Arc<Mutex<Vec<u8>>>,
    trace_id: &str,
    budget: Duration,
) -> String {
    wait_for_line_matching(buffer, budget, |line| {
        line.contains(trace_id) && line.contains("http.status=")
    })
    .await
}

/// A `MakeWriter` that appends every formatted record to one buffer.
#[derive(Clone)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        lock(&self.0).extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for Sink {
    type Writer = Self;

    fn make_writer(&'writer self) -> Self::Writer {
        self.clone()
    }
}

/// Locks the buffer, recovering it when a previous holder panicked.
fn lock(buffer: &Mutex<Vec<u8>>) -> MutexGuard<'_, Vec<u8>> {
    buffer.lock().unwrap_or_else(PoisonError::into_inner)
}
