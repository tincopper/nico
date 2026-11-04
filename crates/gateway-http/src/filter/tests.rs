use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

use async_trait::async_trait;
use pingora::http::{RequestHeader, ResponseHeader, StatusCode};

use super::{FilterChain, HttpFilter};
use crate::{
    context::HttpContext,
    error::HttpError,
    test_support::{request_to, snapshot},
};

const ROUTE: &str = concat!(
    "  - id: api\n",
    "    path_prefix: /\n",
    "    upstream: api-backend\n",
);

/// A filter that records `name:route_id` when it runs, and fails when asked to.
struct Recorder {
    name: &'static str,
    fails: bool,
    calls: Arc<Mutex<Vec<String>>>,
}

impl Recorder {
    fn new(name: &'static str, calls: &Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            name,
            fails: false,
            calls: Arc::clone(calls),
        }
    }

    fn failing(name: &'static str, calls: &Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            name,
            fails: true,
            calls: Arc::clone(calls),
        }
    }

    fn record(&self, ctx: &HttpContext) {
        let route_id = ctx.route_id().unwrap_or("-");
        self.calls
            .lock()
            .expect("calls lock")
            .push(format!("{}:{route_id}", self.name));
    }

    fn reject(&self) -> Result<(), HttpError> {
        if self.fails {
            return Err(HttpError::InvalidRequest {
                reason: self.name.into(),
            });
        }
        Ok(())
    }
}

#[async_trait]
impl HttpFilter for Recorder {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn on_request(
        &self,
        ctx: &mut HttpContext,
        _request: &mut RequestHeader,
    ) -> Result<(), HttpError> {
        self.record(ctx);
        self.reject()
    }

    async fn on_response(
        &self,
        ctx: &mut HttpContext,
        _response: &mut ResponseHeader,
    ) -> Result<(), HttpError> {
        self.record(ctx);
        self.reject()
    }
}

/// A filter that rewrites the request headers, proving the chain hands out mutable access.
struct AddsHeader;

#[async_trait]
impl HttpFilter for AddsHeader {
    fn name(&self) -> &'static str {
        "adds-header"
    }

    async fn on_request(
        &self,
        _ctx: &mut HttpContext,
        request: &mut RequestHeader,
    ) -> Result<(), HttpError> {
        request
            .insert_header("x-filtered", "yes")
            .map_err(|_| HttpError::InvalidRequest {
                reason: "x-filtered header is not writable".into(),
            })
    }

    async fn on_response(
        &self,
        _ctx: &mut HttpContext,
        response: &mut ResponseHeader,
    ) -> Result<(), HttpError> {
        response
            .insert_header("x-filtered", "yes")
            .map_err(|_| HttpError::InvalidRequest {
                reason: "x-filtered header is not writable".into(),
            })
    }
}

/// Builds a context for [`ROUTE`] and the request it is matched against.
fn started() -> (HttpContext, RequestHeader) {
    let snapshot = snapshot(ROUTE);
    let route = &snapshot.document().routes()[0];
    let request = request_to("example.test", "/items");
    let mut ctx = HttpContext::default();
    ctx.begin(&snapshot, route, &request, Instant::now());
    (ctx, request)
}

fn chain(filters: Vec<Arc<dyn HttpFilter>>) -> FilterChain {
    FilterChain::new(Arc::from(filters))
}

/// A filter that never returns, so only the request's budget can end the stage.
struct NeverFinishes;

#[async_trait]
impl HttpFilter for NeverFinishes {
    fn name(&self) -> &'static str {
        "never-finishes"
    }

    async fn on_request(
        &self,
        _ctx: &mut HttpContext,
        _request: &mut RequestHeader,
    ) -> Result<(), HttpError> {
        std::future::pending().await
    }

    async fn on_response(
        &self,
        _ctx: &mut HttpContext,
        _response: &mut ResponseHeader,
    ) -> Result<(), HttpError> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn request_filters_run_in_chain_order() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let chain = chain(vec![
        Arc::new(Recorder::new("first", &calls)),
        Arc::new(Recorder::new("second", &calls)),
    ]);
    let (mut ctx, mut request) = started();

    chain
        .run_request(&mut ctx, &mut request)
        .await
        .expect("no filter rejects");

    assert_eq!(
        *calls.lock().expect("calls lock"),
        ["first:api", "second:api"]
    );
}

#[tokio::test]
async fn response_filters_run_in_chain_order() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let chain = chain(vec![
        Arc::new(Recorder::new("first", &calls)),
        Arc::new(Recorder::new("second", &calls)),
    ]);
    let (mut ctx, _) = started();
    let mut response = ResponseHeader::build(StatusCode::OK, None).expect("response builds");

    chain
        .run_response(&mut ctx, &mut response)
        .await
        .expect("no filter rejects");

    assert_eq!(
        *calls.lock().expect("calls lock"),
        ["first:api", "second:api"]
    );
}

#[tokio::test]
async fn the_first_request_failure_short_circuits_the_chain() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let chain = chain(vec![
        Arc::new(Recorder::new("first", &calls)),
        Arc::new(Recorder::failing("second", &calls)),
        Arc::new(Recorder::new("third", &calls)),
    ]);
    let (mut ctx, mut request) = started();

    let error = chain
        .run_request(&mut ctx, &mut request)
        .await
        .expect_err("the second filter rejects");

    assert!(
        matches!(&error, HttpError::InvalidRequest { reason } if reason == "second"),
        "the failing filter's error must propagate unchanged, got {error:?}"
    );
    assert_eq!(
        *calls.lock().expect("calls lock"),
        ["first:api", "second:api"],
        "the third filter must not run"
    );
}

#[tokio::test]
async fn the_first_response_failure_short_circuits_the_chain() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let chain = chain(vec![
        Arc::new(Recorder::failing("first", &calls)),
        Arc::new(Recorder::new("second", &calls)),
    ]);
    let (mut ctx, _) = started();
    let mut response = ResponseHeader::build(StatusCode::OK, None).expect("response builds");

    let error = chain
        .run_response(&mut ctx, &mut response)
        .await
        .expect_err("the first filter rejects");

    assert!(matches!(error, HttpError::InvalidRequest { .. }));
    assert_eq!(*calls.lock().expect("calls lock"), ["first:api"]);
}

#[tokio::test]
async fn an_empty_chain_accepts_every_request_and_response() {
    let chain = FilterChain::empty();
    let (mut ctx, mut request) = started();
    let mut response = ResponseHeader::build(StatusCode::OK, None).expect("response builds");

    assert!(chain.run_request(&mut ctx, &mut request).await.is_ok());
    assert!(chain.run_response(&mut ctx, &mut response).await.is_ok());
}

#[test]
fn the_default_chain_runs_no_filters() {
    let chain = FilterChain::default();
    let (mut ctx, mut request) = started();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime builds");
    runtime
        .block_on(chain.run_request(&mut ctx, &mut request))
        .expect("the default chain accepts the request");
    assert!(request.headers.get("x-filtered").is_none());
}

#[tokio::test(start_paused = true)]
async fn a_request_filter_that_outlives_the_budget_is_stopped_by_the_chain() {
    // The clock is paused, so the 30 s fixture budget elapses instantly instead of being slept
    // through: the assertion is about which failure comes back, not about waiting.
    let chain = chain(vec![Arc::new(NeverFinishes)]);
    let (mut ctx, mut request) = started();

    let error = chain
        .run_request(&mut ctx, &mut request)
        .await
        .expect_err("the budget, not the filter, ends the stage");

    assert!(
        matches!(error, HttpError::DeadlineExceeded { .. }),
        "a policy that awaits for ever must end as the request's deadline, got {error:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_response_filter_that_outlives_the_budget_is_stopped_by_the_chain() {
    let chain = chain(vec![Arc::new(NeverFinishes)]);
    let (mut ctx, _) = started();
    let mut response = ResponseHeader::build(StatusCode::OK, None).expect("response builds");

    let error = chain
        .run_response(&mut ctx, &mut response)
        .await
        .expect_err("the budget, not the filter, ends the stage");

    assert!(matches!(error, HttpError::DeadlineExceeded { .. }));
}

#[tokio::test]
async fn a_filter_can_rewrite_the_request_and_response_headers() {
    let chain = chain(vec![Arc::new(AddsHeader)]);
    let (mut ctx, mut request) = started();
    let mut response = ResponseHeader::build(StatusCode::OK, None).expect("response builds");

    chain
        .run_request(&mut ctx, &mut request)
        .await
        .expect("the filter accepts");
    chain
        .run_response(&mut ctx, &mut response)
        .await
        .expect("the filter accepts");

    assert_eq!(
        request.headers.get("x-filtered"),
        Some(&"yes".parse().expect("value"))
    );
    assert_eq!(
        response.headers.get("x-filtered"),
        Some(&"yes".parse().expect("value"))
    );
}
