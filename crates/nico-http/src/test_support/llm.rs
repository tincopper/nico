//! The LLM seam's fixtures: a published front-end, a recording fake, and the document they serve.
//!
//! Kept beside [`super`] rather than in it because the two halves answer different questions: the
//! parent builds documents and snapshots, this one drives the seam the way the proxy hooks do. A
//! front-end is a trait object this crate reaches only through an `Arc`, so a fixture that records
//! what it was asked is the only way to assert this crate's half of the contract.

use std::{
    fmt,
    sync::{Arc, Mutex, MutexGuard},
    time::Instant,
};

use nico_core::{ConfigDigest, Document, Generation, ParseLimits, Revision, RuntimeSnapshot};

use super::request;
use crate::{
    Destination,
    context::HttpContext,
    error::HttpError,
    llm::{
        AttemptTarget, AttemptVerdict, ConvertingRoute, LlmAttempt, LlmPrepared, LlmRequest,
        LlmResponseHead, LlmRoute, LlmRoutes, ResponseTransform,
    },
};

/// The provider every LLM fixture targets.
pub(crate) const PROVIDER: &str = concat!(
    "  - name: openai-primary\n",
    "    kind: openai\n",
    "    base_url: http://127.0.0.1:9000\n",
    "    credential_ref: env:FIXTURE_KEY\n",
);

/// A route whose target only an LLM front-end can resolve.
pub(crate) const LLM_ROUTE: &str = concat!(
    "  - id: ai\n",
    "    path_prefix: /v1/\n",
    "    provider: openai-primary\n",
    "    protocol: openai.chat\n",
);

/// Builds a snapshot of [`LLM_ROUTE`] with the fixture document and route ceilings.
///
/// The document accepts 4096 bytes of buffered response; a route that wants less splices
/// `    max_response_bytes: <n>` through [`llm_snapshot_with`].
pub(crate) fn llm_snapshot() -> Arc<RuntimeSnapshot> {
    llm_snapshot_with("")
}

/// Builds the LLM fixture with extra route lines, indented like the document.
pub(crate) fn llm_snapshot_with(route_fields: &str) -> Arc<RuntimeSnapshot> {
    let yaml = format!(
        concat!(
            "apiVersion: gateway/v1\n",
            "request:\n",
            "  max_body_bytes: 1048576\n",
            "  max_response_bytes: 4096\n",
            "  timeout_ms: 30000\n",
            "plugins: []\n",
            "providers:\n",
            "{}\n",
            "upstreams: []\n",
            "routes:\n",
            "{}{}\n",
        ),
        PROVIDER, LLM_ROUTE, route_fields
    );
    let document =
        Document::parse(yaml.as_bytes(), ParseLimits::new(64 * 1024)).expect("fixture is valid");
    Arc::new(RuntimeSnapshot::new(
        Generation::new(1),
        Revision::comparable(1),
        ConfigDigest::of(yaml.as_bytes()),
        document,
        "test",
    ))
}

/// Starts a request that already matched the LLM fixture's route, with `front_end` installed.
pub(crate) fn llm_context(
    snapshot: &Arc<RuntimeSnapshot>,
    front_end: Arc<dyn LlmRoute>,
) -> HttpContext {
    let route = &snapshot.document().routes()[0];
    let mut ctx = HttpContext::default();
    ctx.begin(
        snapshot,
        &request("POST", "/v1/chat/completions"),
        Instant::now(),
    );
    ctx.bind_route(route);
    ctx.set_trace_id("trace-llm");
    ctx.set_llm(front_end);
    ctx
}

/// One `prepare` call, as the fake front-end saw it.
#[derive(Debug, Default)]
pub(crate) struct PreparedCall {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

/// Everything a fake front-end was asked to do, so a test can assert this crate's side of the seam.
#[derive(Debug, Default)]
pub(crate) struct LlmCalls {
    pub(crate) prepared: Vec<PreparedCall>,
    /// The `(streaming, max_response_bytes)` each created transform was handed.
    pub(crate) transforms: Vec<(bool, u32)>,
    /// The `(status, headers)` each transform was asked to translate.
    pub(crate) heads: Vec<(u16, Vec<(String, String)>)>,
    /// Every chunk a transform was asked to translate, in order.
    pub(crate) chunks: Vec<Vec<u8>>,
    pub(crate) finishes: usize,
}

/// A front-end that answers fixed values and records what it was asked.
pub(crate) struct FakeLlmRoute {
    pub(crate) converting_route: bool,
    pub(crate) destination: Destination,
    pub(crate) path: String,
    pub(crate) headers: Vec<(String, String)>,
    /// The bytes `prepare` answers with and the stream flag it reports.
    pub(crate) body: Vec<u8>,
    pub(crate) streaming: bool,
    pub(crate) model: Option<String>,
    /// `Some(reason)` makes `prepare` answer `400`.
    pub(crate) fail_prepare: Option<&'static str>,
    /// Whether the route declared attempts, which is the walk's answer to `can_retry`.
    pub(crate) retries: bool,
    /// The head the transform answers with; `None` makes the transform fail on the head.
    pub(crate) head: Option<(u16, Vec<(String, String)>)>,
    pub(crate) calls: Arc<Mutex<LlmCalls>>,
}

impl FakeLlmRoute {
    /// Builds a converting front-end for `127.0.0.1:9000` that answers a 200 JSON head.
    pub(crate) fn new(calls: &Arc<Mutex<LlmCalls>>) -> Self {
        Self {
            converting_route: true,
            destination: Destination::new(nico_core::Scheme::Http, "127.0.0.1", 9000),
            path: "/v1/chat/completions".to_owned(),
            headers: vec![
                ("authorization".to_owned(), "Bearer provider-key".to_owned()),
                ("content-type".to_owned(), "application/json".to_owned()),
            ],
            body: b"{\"model\":\"gpt-physical\"}".to_vec(),
            streaming: false,
            model: Some("fast -> gpt-physical".to_owned()),
            fail_prepare: None,
            retries: false,
            head: Some((
                200,
                vec![("content-type".to_owned(), "application/json".to_owned())],
            )),
            calls: Arc::clone(calls),
        }
    }

    /// Declares attempts, so the retry gate has a walk that admits another one.
    pub(crate) fn with_retries(mut self) -> Self {
        self.retries = true;
        self
    }

    /// Returns the recorded calls.
    pub(crate) fn calls(&self) -> MutexGuard<'_, LlmCalls> {
        self.calls.lock().expect("the fake lock is not poisoned")
    }
}

impl LlmRoute for FakeLlmRoute {
    fn converting(&self) -> Option<&dyn ConvertingRoute> {
        self.converting_route.then_some(self)
    }

    fn mode_name(&self) -> &'static str {
        if self.converting_route {
            "convert"
        } else {
            "passthrough"
        }
    }

    fn protocol(&self) -> &str {
        "openai.chat"
    }

    fn provider(&self) -> &str {
        "openai"
    }

    fn destination(&self) -> &Destination {
        &self.destination
    }

    fn path(&self) -> &str {
        &self.path
    }

    fn headers(&self) -> &[(String, String)] {
        &self.headers
    }
}

impl ConvertingRoute for FakeLlmRoute {
    fn begin(&self) -> Box<dyn LlmAttempt> {
        // The walk answers every classification as the response the data plane should translate, and
        // only a route built with `with_retries` reports that it declared attempts. That is what the
        // phase-1 hook tests assert, and what a retry-gate test needs to be able to vary. It reproduces
        // `prepare` so the hooks that go through the walk record the same calls the direct path did.
        Box::new(FakeAttempt {
            target: AttemptTarget::new(
                self.destination.clone(),
                self.path.clone(),
                self.headers.clone(),
                "openai",
            ),
            body: self.body.clone(),
            streaming: self.streaming,
            model: self.model.clone(),
            fail_prepare: self.fail_prepare,
            retries: self.retries,
            calls: Arc::clone(&self.calls),
        })
    }

    fn prepare(&self, request: &LlmRequest<'_>) -> Result<LlmPrepared, HttpError> {
        self.calls().prepared.push(PreparedCall {
            method: request.method().to_owned(),
            path: request.path().to_owned(),
            headers: request
                .headers()
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            body: request.body().to_vec(),
        });
        if let Some(reason) = self.fail_prepare {
            return Err(HttpError::InvalidRequest {
                reason: reason.into(),
            });
        }
        let prepared = LlmPrepared::new(self.body.clone(), self.streaming);
        Ok(match &self.model {
            Some(model) => prepared.with_model(model.clone()),
            None => prepared,
        })
    }

    fn transform(
        &self,
        streaming: bool,
        max_response_bytes: u32,
    ) -> Result<Box<dyn ResponseTransform>, HttpError> {
        self.calls()
            .transforms
            .push((streaming, max_response_bytes));
        Ok(Box::new(FakeTransform {
            head: self.head.clone(),
            limit: max_response_bytes,
            total: 0,
            calls: Arc::clone(&self.calls),
        }))
    }
}

/// A transform that uppercases every chunk, completes with `<end>`, and enforces the ceiling.
struct FakeTransform {
    head: Option<(u16, Vec<(String, String)>)>,
    limit: u32,
    total: usize,
    calls: Arc<Mutex<LlmCalls>>,
}

impl FakeTransform {
    /// Records one call, so the assertions can see the order the data plane drove.
    fn calls(&self) -> MutexGuard<'_, LlmCalls> {
        self.calls.lock().expect("the fake lock is not poisoned")
    }
}

impl ResponseTransform for FakeTransform {
    fn head(
        &mut self,
        status: u16,
        headers: &[(String, String)],
    ) -> Result<LlmResponseHead, HttpError> {
        self.calls().heads.push((status, headers.to_vec()));
        let Some((status, headers)) = &self.head else {
            return Err(HttpError::Internal {
                source: Box::new(std::io::Error::other("the fake transform refused the head")),
            });
        };
        LlmResponseHead::try_new(*status, headers.clone())
    }

    fn push(&mut self, chunk: &[u8]) -> Result<Vec<u8>, HttpError> {
        self.calls().chunks.push(chunk.to_vec());
        self.total = self.total.saturating_add(chunk.len());
        if crate::budget::over_limit(self.limit, self.total as u64) {
            return Err(HttpError::UpstreamResponseTooLarge { limit: self.limit });
        }
        Ok(chunk.to_ascii_uppercase())
    }

    fn finish(&mut self) -> Result<Vec<u8>, HttpError> {
        self.calls().finishes += 1;
        Ok(b"<end>".to_vec())
    }
}

impl fmt::Debug for FakeTransform {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FakeTransform")
            .field("limit", &self.limit)
            .field("total", &self.total)
            .finish()
    }
}

/// A published plan with the routes a test names.
pub(crate) struct FakeLlmRoutes {
    routes: Vec<(String, Arc<dyn LlmRoute>)>,
}

impl FakeLlmRoutes {
    /// Returns a plan that serves no route.
    pub(crate) fn empty() -> Arc<dyn LlmRoutes> {
        Arc::new(Self { routes: Vec::new() })
    }

    /// Returns a plan with one route.
    pub(crate) fn one(route_id: &str, route: Arc<dyn LlmRoute>) -> Arc<dyn LlmRoutes> {
        Arc::new(Self {
            routes: vec![(route_id.to_owned(), route)],
        })
    }
}

impl LlmRoutes for FakeLlmRoutes {
    fn route(&self, route_id: &str) -> Option<Arc<dyn LlmRoute>> {
        self.routes
            .iter()
            .find(|(id, _)| id == route_id)
            .map(|(_, route)| Arc::clone(route))
    }

    fn len(&self) -> usize {
        self.routes.len()
    }
}

/// The fixture's attempt walk: one target, one attempt, and every head translated as the answer.
///
/// The hook tests exercise the data plane's framing and plugin ordering, not the attempt sequence, so
/// this walk never asks for another attempt. The sequence itself is covered in `nico-llm`. Its
/// `prepare` mirrors the fixture route's, so a test asserting the recorded call sees the same facts
/// whether the hook reached the front-end directly or through the walk.
struct FakeAttempt {
    target: AttemptTarget,
    body: Vec<u8>,
    streaming: bool,
    model: Option<String>,
    fail_prepare: Option<&'static str>,
    retries: bool,
    calls: Arc<Mutex<LlmCalls>>,
}

impl LlmAttempt for FakeAttempt {
    fn select(&mut self, _now: tokio::time::Instant) -> Result<AttemptTarget, HttpError> {
        Ok(self.target.clone())
    }

    fn prepare(
        &mut self,
        request: &LlmRequest<'_>,
        _now: tokio::time::Instant,
    ) -> Result<LlmPrepared, HttpError> {
        self.calls
            .lock()
            .expect("the fake lock is not poisoned")
            .prepared
            .push(PreparedCall {
                method: request.method().to_owned(),
                path: request.path().to_owned(),
                headers: request
                    .headers()
                    .iter()
                    .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                    .collect(),
                body: request.body().to_vec(),
            });
        if let Some(reason) = self.fail_prepare {
            return Err(HttpError::InvalidRequest {
                reason: reason.into(),
            });
        }
        let prepared = LlmPrepared::new(self.body.clone(), self.streaming);
        Ok(match &self.model {
            Some(model) => prepared.with_model(model.clone()),
            None => prepared,
        })
    }

    fn classify(
        &mut self,
        _status: u16,
        _now: tokio::time::Instant,
    ) -> Result<AttemptVerdict, HttpError> {
        Ok(AttemptVerdict::Answer)
    }

    fn failed(&mut self, _now: tokio::time::Instant) -> Result<AttemptVerdict, HttpError> {
        Ok(AttemptVerdict::Answer)
    }

    fn succeeded(&mut self, _now: tokio::time::Instant) {}

    fn can_retry(&self) -> bool {
        self.retries
    }

    fn current_target(&self) -> &str {
        self.target.name()
    }

    fn ttft(&self) -> Option<std::time::Duration> {
        self.target.ttft()
    }

    fn streaming(&self) -> Option<bool> {
        Some(self.streaming)
    }

    fn attempt_number(&self) -> u32 {
        1
    }

    fn target_view(&self) -> AttemptTarget {
        self.target.clone()
    }

    fn body(&self) -> Option<&[u8]> {
        // The fixture's walk never retries, so no replay round ever asks it for a body.
        None
    }

    fn last_refusal(&self) -> Option<HttpError> {
        // The fixture's walk never retries, so it never produces the signal this answers.
        None
    }

    fn take_skipped(&mut self) -> Vec<String> {
        Vec::new()
    }
}
