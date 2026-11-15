//! A scripted `EtcdClient`: the "real environment" these tests run against.
//!
//! Each test scripts the `get` answers and the watch outcomes in order, then asserts on what the
//! adapter did with them. The script is the server; nothing here connects anywhere.

use std::sync::Mutex;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::mpsc;

use crate::{EtcdClient, EtcdError, KvEntry, WatchOutcome, WatchStream};

/// A scripted client: `get` answers come off one queue, watch outcomes off another.
///
/// The watch outcomes are delivered through a channel so a test can also exercise "the stream ends"
/// and "the server sends events later" without polling puzzles.
#[derive(Debug, Default)]
pub(crate) struct MockClient {
    gets: Mutex<Vec<Result<Option<KvEntry>, EtcdError>>>,
    watch_script: Mutex<Vec<ScriptedWatch>>,
    /// The keys each `watch` call asked for, for the start-revision assertions.
    watched: Mutex<Vec<(String, u64)>>,
}

/// One scripted watch: the outcomes it yields, in order, then the stream ends.
#[derive(Debug)]
pub(crate) struct ScriptedWatch {
    outcomes: Vec<WatchOutcome>,
}

impl ScriptedWatch {
    /// Wraps one outcome batch into a scripted watch.
    pub(crate) fn once(outcomes: Vec<WatchOutcome>) -> Self {
        Self { outcomes }
    }
}

impl MockClient {
    /// Scripts the next `get` answer.
    pub(crate) fn script_get(&self, answer: Result<Option<KvEntry>, EtcdError>) {
        self.gets.lock().expect("not poisoned").push(answer);
    }

    /// Scripts the next watch's outcomes.
    pub(crate) fn script_watch(&self, script: ScriptedWatch) {
        self.watch_script.lock().expect("not poisoned").push(script);
    }

    /// Returns the `(key, start_revision)` pairs the adapter asked to watch.
    pub(crate) fn watched(&self) -> Vec<(String, u64)> {
        self.watched.lock().expect("not poisoned").clone()
    }
}

#[async_trait]
impl EtcdClient for MockClient {
    async fn get(&self, _key: &str) -> Result<Option<KvEntry>, EtcdError> {
        self.gets
            .lock()
            .expect("not poisoned")
            .pop()
            .expect("the test scripted a get for every load")
    }

    async fn watch(&self, key: &str, start_revision: u64) -> Result<WatchStream, EtcdError> {
        self.watched
            .lock()
            .expect("not poisoned")
            .push((key.to_owned(), start_revision));
        let script = self
            .watch_script
            .lock()
            .expect("not poisoned")
            .pop()
            .expect("the test scripted a watch for every subscription");
        let (sender, receiver) = mpsc::channel(script.outcomes.len().max(1));
        tokio::spawn(async move {
            for outcome in script.outcomes {
                if sender.send(Ok(outcome)).await.is_err() {
                    return;
                }
            }
        });
        Ok(receiver)
    }
}

/// Builds a `KvEntry` for one revision.
pub(crate) fn entry(revision: u64, value: &str) -> Option<KvEntry> {
    Some(KvEntry {
        value: Bytes::from(value.to_owned()),
        mod_revision: revision,
    })
}

/// The default document budget.
pub(crate) const MAX_BYTES: usize = 64 * 1024;

/// The key every test watches.
pub(crate) const MOCK_KEY: &str = "/nico/config.yaml";
