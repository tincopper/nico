use pingora::server::{ShutdownSignal, ShutdownSignalWatch};
use tokio_util::sync::CancellationToken;

use super::CancelWatch;

#[tokio::test(flavor = "current_thread")]
async fn a_cancelled_token_becomes_a_graceful_termination() {
    let shutdown = CancellationToken::new();
    let watch = CancelWatch::new(shutdown.clone());
    shutdown.cancel();

    let signal = watch.recv().await;

    assert!(
        matches!(signal, ShutdownSignal::GracefulTerminate),
        "the token must ask Pingora to drain, never to terminate abruptly"
    );
    assert!(
        shutdown.is_cancelled(),
        "the watcher cancels the shared token, which is what stops the reload pipeline"
    );
}
