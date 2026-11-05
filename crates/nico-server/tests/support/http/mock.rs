//! A plugin kind that exists only in tests: it does exactly what the fixture says and records when.
//!
//! Four scenarios need something no built-in kind provides — evidence about the *driver*: that a
//! global chain ran before a route was matched, that a buffered body arrives once and whole, that a
//! synthesized response is cleaned by the writer, that a response plugin sees one chunk at a time.
//! Asserting those against a real policy would tie the assertion to that policy's refusals; asserting
//! them against a mock ties it to the plugin contract, which is the subject.
//!
//! This kind is registered by the test harness only ([`super::RunningServer::start_with_registry`]).
//! It is never part of `nico_plugins::builtin_registry()`, so a document naming `mock` in a deployment
//! is a publication failure, not a policy.
//!
//! The configuration is the four actions below, one per scenario, parsed with `deny_unknown_fields`
//! exactly as a built-in kind does:
//!
//! | `action` | stage | what it proves |
//! |---|---|---|
//! | `tag` | `on_request` | the order the scopes ran in, observable in the upstream's headers |
//! | `respond` | `on_request` | a synthesized response, cleaned and written instead of forwarded |
//! | `rewrite_body` | `on_request_body` | one call with the whole body — a buffered route |
//! | `append_response` | `on_response_body` | one call per downstream chunk, never the whole body |
//!
//! Every call is recorded in a [`MockRecorder`] the test holds, and [`MockRecorder::wait_for`] waits
//! on that record instead of on a sleep.

use std::{
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use nico_core::{ErrorCategory, PluginSpec};
use nico_http::{
    Outcome, Plugin, PluginBuildError, PluginContext, PluginError, PluginFactory, PluginInfo,
    PluginPhase, PluginPhases, PluginResult, ResponseData,
};
use serde::Deserialize;
use tokio::sync::watch;

/// The document `kind` this module answers to.
const KIND: &str = "mock";

/// A minimal configuration this kind accepts, as JSON text.
///
/// The factory and every instance report the same string, because
/// [`PluginFactory::default_config`] is what documentation shows and [`PluginInfo`] what a plugin
/// says about itself: two spellings of one example would eventually disagree.
const DEFAULT_CONFIG: &str = r#"{"action":"tag","header":"x-mock","value":"seen"}"#;

/// One recorded stage call, attributed to the instance that made it.
///
/// The instance name is recorded rather than inferred: a document may run two entries of this kind in
/// two scopes, and "which one saw this chunk" is exactly what a scope-order assertion asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Event {
    /// `on_request` ran.
    Request {
        /// The instance name the document gave the plugin.
        plugin: String,
    },
    /// `on_request_body` ran, with the size of the chunk it was handed.
    RequestBody {
        /// The instance name the document gave the plugin.
        plugin: String,
        /// Bytes in the chunk, before the plugin rewrote it.
        len: usize,
        /// Whether the driver said this chunk ended the body.
        complete: bool,
    },
    /// `on_response_body` ran, with the size of the chunk it was handed.
    ResponseBody {
        /// The instance name the document gave the plugin.
        plugin: String,
        /// Bytes in the chunk, before the plugin rewrote it.
        len: usize,
    },
}

impl Event {
    /// Returns the instance name the call is attributed to.
    pub(crate) fn plugin(&self) -> &str {
        match self {
            Self::Request { plugin }
            | Self::RequestBody { plugin, .. }
            | Self::ResponseBody { plugin, .. } => plugin,
        }
    }
}

/// The calls every mock instance of one server recorded, and a notification for each new one.
///
/// A single recorder is shared by every instance in the document, which is what makes "global then
/// route" a statement about one ordered sequence rather than about two independent ones.
pub(crate) struct MockRecorder {
    events: Mutex<Vec<Event>>,
    changed: watch::Sender<u64>,
}

impl MockRecorder {
    /// Creates an empty recorder.
    pub(crate) fn new() -> Arc<Self> {
        let (changed, _receiver) = watch::channel(0u64);
        Arc::new(Self {
            events: Mutex::new(Vec::new()),
            changed,
        })
    }

    /// Returns the calls recorded so far, in the order they happened.
    pub(crate) fn events(&self) -> Vec<Event> {
        lock(&self.events).clone()
    }

    /// Waits until `condition` holds for the recorded calls, and returns them.
    ///
    /// The wait is driven by the recorder's own notification, so a scenario never guesses how long a
    /// stage takes; a condition that never holds fails the test with the calls that did happen.
    pub(crate) async fn wait_for(
        &self,
        budget: Duration,
        condition: impl Fn(&[Event]) -> bool,
    ) -> Vec<Event> {
        let mut changed = self.changed.subscribe();
        let deadline = Instant::now() + budget;
        loop {
            let events = self.events();
            if condition(&events) {
                return events;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "the mock plugin did not record the expected call within {budget:?}: {events:?}"
            );
            if tokio::time::timeout(remaining, changed.changed())
                .await
                .is_err()
            {
                let events = self.events();
                panic!(
                    "the mock plugin did not record the expected call within {budget:?}: {events:?}"
                );
            }
        }
    }

    /// Appends one call and wakes every waiter.
    fn record(&self, event: Event) {
        {
            let mut events = lock(&self.events);
            events.push(event);
        }
        // `send_modify` notifies even when no subscriber is listening, so a scenario that waits after
        // the call has already happened still sees the record.
        self.changed.send_modify(|sequence| *sequence += 1);
    }
}

/// Locks the event list, recovering it when a previous holder panicked.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The loading seam for the `mock` kind: every instance shares one recorder.
pub(crate) struct MockFactory {
    recorder: Arc<MockRecorder>,
}

impl MockFactory {
    /// Returns the factory a test registers, sharing `recorder` with the test itself.
    pub(crate) fn new(recorder: Arc<MockRecorder>) -> Arc<Self> {
        Arc::new(Self { recorder })
    }
}

impl PluginFactory for MockFactory {
    fn kind(&self) -> &'static str {
        KIND
    }

    /// Neither authenticates nor charges: a mock has no ordering opinion of its own, so the document's
    /// order decides where it runs among the other `Other` kinds.
    fn phase(&self) -> PluginPhase {
        PluginPhase::Other
    }

    fn default_config(&self) -> &'static str {
        DEFAULT_CONFIG
    }

    fn build(&self, spec: &PluginSpec) -> Result<Arc<dyn Plugin>, PluginBuildError> {
        let config: MockConfig = config_of(spec)?;
        let plugin = Mock {
            name: spec.name().to_owned(),
            action: config.action,
            header: config.header,
            value: config.value,
            prefix: config.prefix,
            suffix: config.suffix,
            status: config.status.unwrap_or(DEFAULT_STATUS),
            headers: config.headers,
            body: config.body.into_bytes(),
            recorder: Arc::clone(&self.recorder),
        };
        plugin.validate(spec)?;
        Ok(Arc::new(plugin))
    }
}

/// The four things a mock instance can do, one per scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    /// Add `value` to the `header` request header, after its existing value.
    Tag,
    /// Answer the request with `status`, `headers` and `body`.
    Respond,
    /// Rewrite the whole request body as `prefix + body + suffix`.
    RewriteBody,
    /// Append `suffix` to every response-body chunk.
    AppendResponse,
}

/// One mock entry's configuration, read with `deny_unknown_fields` while the document is compiled.
///
/// One struct with an [`Action`] field rather than a tagged enum: a tagged enum cannot reject unknown
/// fields, and "an unknown configuration field is refused at publication" is the rule every kind has to
/// obey — including this one, or a typo in a fixture would silently do nothing.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MockConfig {
    /// What the instance does.
    action: Action,
    /// The request header `tag` reads and writes.
    #[serde(default)]
    header: String,
    /// The value `tag` appends.
    #[serde(default)]
    value: String,
    /// What `rewrite_body` puts before the body.
    #[serde(default)]
    prefix: String,
    /// What `rewrite_body` puts after the body, or `append_response` after every chunk.
    #[serde(default)]
    suffix: String,
    /// The status `respond` writes; a scenario writes one when it varies it.
    #[serde(default)]
    status: Option<u16>,
    /// The headers `respond` writes, in order.
    #[serde(default)]
    headers: Vec<(String, String)>,
    /// The body `respond` writes, or `append_response`'s only content.
    #[serde(default)]
    body: String,
}

/// The status `respond` writes when the entry does not choose one: "an answer", not a refusal.
const DEFAULT_STATUS: u16 = 200;

/// One plugin instance: the action, its parameters, and the recorder it appends to.
struct Mock {
    name: String,
    action: Action,
    header: String,
    value: String,
    prefix: String,
    suffix: String,
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    recorder: Arc<MockRecorder>,
}

impl Mock {
    /// Checks the fields the action needs, at publication rather than on the request path.
    fn validate(&self, spec: &PluginSpec) -> Result<(), PluginBuildError> {
        if self.action == Action::Tag && (self.header.is_empty() || self.value.is_empty()) {
            return Err(PluginBuildError::new(
                spec.name(),
                "`tag` needs both `header` and `value`",
            )
            .with_category(ErrorCategory::Client));
        }
        Ok(())
    }

    /// Records one call, attributed to this instance.
    fn record(&self, event: impl FnOnce(String) -> Event) {
        self.recorder.record(event(self.name.clone()));
    }

    /// Reports the fixture's own defect as a plugin failure, never as a panic.
    fn failed(reason: &'static str) -> PluginError {
        PluginError::Execute(reason)
    }
}

#[async_trait]
impl Plugin for Mock {
    fn name(&self) -> &str {
        &self.name
    }

    fn info(&self) -> PluginInfo {
        // The declaration is what tells the data plane how the response may be framed: a kind that
        // rewrites response bytes must say so, or the upstream's `content-length` would describe bytes
        // that are no longer sent.
        let phases = match self.action {
            Action::RewriteBody => PluginPhases::REQUEST | PluginPhases::REQUEST_BODY,
            Action::AppendResponse => PluginPhases::REQUEST | PluginPhases::RESPONSE_BODY,
            Action::Tag | Action::Respond => PluginPhases::REQUEST,
        };
        PluginInfo::new("test", "test-only mock plugin", DEFAULT_CONFIG).with_phases(phases)
    }

    async fn on_request(&self, ctx: &mut dyn PluginContext) -> PluginResult {
        match self.action {
            Action::Tag => {
                self.record(|plugin| Event::Request { plugin });
                let existing = ctx
                    .request_header(&self.header)
                    .map_err(|_error| Self::failed("the tag header could not be read"))?;
                let combined = match existing {
                    Some(previous) => format!("{previous},{}", self.value),
                    None => self.value.clone(),
                };
                ctx.set_request_header(&self.header, &combined)
                    .map_err(|_error| Self::failed("the tag header could not be written"))?;
                Ok(Outcome::Continue)
            }
            Action::Respond => {
                self.record(|plugin| Event::Request { plugin });
                let data = ResponseData::new(self.status, self.headers.clone(), self.body.clone())
                    .map_err(|_error| Self::failed("the fixture response cannot be written"))?;
                Ok(Outcome::Respond(data))
            }
            Action::RewriteBody | Action::AppendResponse => Ok(Outcome::Continue),
        }
    }

    async fn on_request_body(&self, ctx: &mut dyn PluginContext) -> PluginResult {
        if self.action != Action::RewriteBody {
            return Ok(Outcome::Continue);
        }
        let body = ctx
            .request_body()
            .map_err(|_error| Self::failed("the request body could not be read"))?;
        let complete = ctx
            .body_complete()
            .map_err(|_error| Self::failed("the body mode could not be read"))?;
        self.record(|plugin| Event::RequestBody {
            plugin,
            len: body.len(),
            complete,
        });
        let mut rewritten = Vec::with_capacity(self.prefix.len() + body.len() + self.suffix.len());
        rewritten.extend_from_slice(self.prefix.as_bytes());
        rewritten.extend_from_slice(&body);
        rewritten.extend_from_slice(self.suffix.as_bytes());
        ctx.set_request_body(rewritten)
            .map_err(|_error| Self::failed("the request body could not be replaced"))?;
        Ok(Outcome::Continue)
    }

    fn on_response_body(&self, ctx: &mut dyn PluginContext) -> PluginResult {
        if self.action != Action::AppendResponse {
            return Ok(Outcome::Continue);
        }
        // Synchronous by contract: the framework hook that delivers a downstream chunk cannot await.
        let body = ctx
            .response_body()
            .map_err(|_error| Self::failed("the response body could not be read"))?;
        self.record(|plugin| Event::ResponseBody {
            plugin,
            len: body.len(),
        });
        let mut rewritten = Vec::with_capacity(body.len() + self.suffix.len());
        rewritten.extend_from_slice(&body);
        rewritten.extend_from_slice(self.suffix.as_bytes());
        ctx.set_response_body(rewritten)
            .map_err(|_error| Self::failed("the response body could not be replaced"))?;
        Ok(Outcome::Continue)
    }
}

/// Reads one entry's `config` value as this kind's configuration.
///
/// An entry that writes no `config` is read as one that writes no fields, so the refusal names the
/// missing `action` instead of "null is not a mapping" — the same normalization the built-in kinds do.
fn config_of(spec: &PluginSpec) -> Result<MockConfig, PluginBuildError> {
    let value = match spec.config() {
        serde_norway::Value::Null => serde_norway::Value::Mapping(serde_norway::Mapping::new()),
        written => written.clone(),
    };
    serde_norway::from_value(value).map_err(|error| {
        PluginBuildError::new(spec.name(), format!("invalid mock configuration: {error}"))
            .with_category(ErrorCategory::Client)
    })
}
