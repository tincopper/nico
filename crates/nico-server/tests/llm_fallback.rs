//! Fallback, retry and the circuit breaker, end to end: real server, real mock origins.
//!
//! Every scenario is a fact about the attempt sequence as the data plane drives it, asserted on the
//! wire: which origin was contacted, how many times, and what the caller was answered. The facts
//! below are the ones a retry implementation can get wrong while every unit test still passes:
//!
//! 1. a retryable failure on the primary target is answered by the fallback, buffered and streamed;
//! 2. a target whose breaker opened is not contacted at all — its connection count stays zero;
//! 3. a credential failure (401) moves to the fallback without spending the budget — the fault is the
//!    member's, not the caller's;
//! 4. a failure after the response head reached the caller is never retried — the stream ends;
//! 5. a body above Pingora's replay buffer is answered, not retried once dispatched;
//! 6. a target whose name does not resolve is walked past — the resolve failure is its transport
//!    failure, and the chain moves on instead of ending the request.
//!
//! One server serves every scenario: a test binary starts exactly one (see `support/http.rs`), and
//! each scenario publishes its own document, then waits for the generation only it can reach.

#[path = "llm_fallback/scenarios.rs"]
mod scenarios;
mod support;

use scenarios::{KEY, base_url, failure, success};
use support::http::{
    RunningServer, free_port,
    resources::{Resources, Route},
    source::TestSource,
    upstream::MockUpstream,
    write_bootstrap,
};

#[tokio::test(flavor = "current_thread")]
async fn the_attempt_sequence_answers_from_the_chain_on_the_wire() {
    // Installed before the server starts: the attempt lines the scenarios pin must be captured.
    let captured = support::http::log::capture();
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let credential = support::token::secret_file(directory.path(), "openai.key", KEY);

    // The first scenario's document is in service at start; the rest publish onto the live server.
    let primary = MockUpstream::start(failure(502)).await;
    let secondary = MockUpstream::start(success("fallback-model")).await;
    let document = Resources::new()
        .provider_with_breaker(
            "primary",
            &base_url(&primary),
            &credential,
            0, // The breaker is disabled here: this scenario is about the budget and the chain.
            30_000,
        )
        .provider_with_breaker("secondary", &base_url(&secondary), &credential, 0, 30_000)
        .route(
            Route::provider("llm-chat", "/v1/chat/", "primary")
                .retry_budget(2)
                .fallback(&["secondary"]),
        )
        .render();
    write_bootstrap(directory.path(), free_port(), false, 1, 1000);
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source.clone()).await;
    let address = server.address();

    scenarios::a_retryable_failure_on_the_primary_is_answered_by_the_fallback(
        &captured, &address, &primary, &secondary,
    )
    .await;
    scenarios::an_open_breaker_is_not_contacted_at_all(&captured, &source, &address, &credential)
        .await;
    scenarios::a_credential_failure_asks_the_fallback_without_burning_the_budget(
        &source,
        &address,
        &credential,
    )
    .await;
    scenarios::a_failure_after_the_head_was_written_does_not_switch_targets(
        &source,
        &address,
        &credential,
    )
    .await;
    scenarios::a_body_above_the_replay_cap_is_not_retried_after_dispatch(
        &source,
        &address,
        &credential,
    )
    .await;
    scenarios::a_target_that_does_not_resolve_is_walked_past(&source, &address, &credential).await;
    scenarios::a_streamed_fallback_answers_the_stream(&source, &address, &credential).await;

    server.cancel_and_join().await;
}
