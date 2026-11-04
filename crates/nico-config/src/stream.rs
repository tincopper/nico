//! A bounded, cancellation-aware stream over an `mpsc` receiver.
//!
//! Both built-in sources push events into a bounded `tokio::sync::mpsc` channel from a background
//! task and expose the receiving side as the contract's [`ConfigWatch`](crate::ConfigWatch). This
//! module is that bridge, so the "drop cancels the task" rule is implemented once.

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use futures_core::Stream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::source::{ConfigEvent, ConfigSourceError};

/// One item of a configuration watch stream.
pub(crate) type WatchItem = Result<ConfigEvent, ConfigSourceError>;

/// The `Stream` half of a source watch.
///
/// The paired [`CancellationToken`] is held by the producing task. Dropping the stream cancels the
/// token, which is how the task learns that nobody is listening any more.
#[derive(Debug)]
pub(crate) struct SourceWatch {
    receiver: mpsc::Receiver<WatchItem>,
    token: CancellationToken,
}

impl SourceWatch {
    /// Wraps `receiver`, cancelling `token` when the stream is dropped.
    pub(crate) fn new(receiver: mpsc::Receiver<WatchItem>, token: CancellationToken) -> Self {
        Self { receiver, token }
    }
}

impl Stream for SourceWatch {
    type Item = WatchItem;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // `SourceWatch` is `Unpin`: both fields are plain owned values.
        self.get_mut().receiver.poll_recv(context)
    }
}

impl Drop for SourceWatch {
    fn drop(&mut self) {
        self.token.cancel();
    }
}
