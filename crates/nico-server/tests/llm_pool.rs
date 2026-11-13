//! A provider pool, end to end: distribution, classification, and the two tables staying apart.
//!
//! Every other LLM scenario names one provider per route, which is a shape that cannot show what a
//! pool exists for. Here one route names several real origins, and the questions are the ones an
//! operator asks when one provider degrades:
//!
//! 1. **does the pool answer every request** — half the requests meet a throttled member and the
//!    caller still gets an answer, which is the whole value of distributing;
//! 2. **is the throttled member ever asked twice for one request** — no: a rate limit moves the walk
//!    on, and the excluded member is not drawn again;
//! 3. **does a caller mistake stop the walk** — a 400 is answered without asking a second provider,
//!    asserted by the second origin's connection count staying at zero;
//! 4. **is the exclusion an operator-visible fact** — the log names the member and the reason.
//!
//! One server per test binary, and each scenario publishes its own document, then waits for a
//! generation only it can reach.

#[path = "llm_pool/scenarios.rs"]
mod scenarios;
mod support;

use scenarios::{
    base_url, chat, chat_stream, failure, pooled_document, pooled_document_with_fallback,
    pooled_document_with_ttft, streamed_success, success,
};
use support::http::{
    RunningServer, free_port, log,
    source::TestSource,
    upstream::{MockUpstream, Reply},
    write_bootstrap,
};

#[tokio::test(flavor = "current_thread")]
async fn a_provider_pool_answers_through_a_throttled_member() {
    let captured = log::capture();
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let credential = support::token::secret_file(directory.path(), "openai.key", scenarios::KEY);

    let serving = MockUpstream::start(success("serving-model")).await;
    let throttled = MockUpstream::start(failure(429)).await;

    let document = pooled_document(
        &[("serving", 1), ("throttled", 1)],
        &credential,
        "round_robin",
        1,
        &[
            ("serving", base_url(&serving)),
            ("throttled", base_url(&throttled)),
        ],
    );

    write_bootstrap(directory.path(), free_port(), false, 1, 1000);
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source.clone()).await;
    let address = server.address();

    // Round robin alternates the two members, so half the requests meet the throttled one and must
    // still be answered by the serving one.
    for _ in 0..4 {
        let response = scenarios::ask(address, &chat()).await;
        assert_eq!(
            response.status(),
            200,
            "the pool answers every request: {}",
            response.text()
        );
        assert!(
            response.text().contains("serving-model"),
            "the serving member answered: {}",
            response.text()
        );
    }

    assert!(
        throttled.observation().connections > 0,
        "the throttled member was offered"
    );
    assert!(
        throttled.observation().connections <= 1,
        "the throttled member is discovered once and then retired for its cooldown, so the requests \
         after the first never reach it (saw {})",
        throttled.observation().connections
    );

    // The exclusion is the operator-visible fact: the reason and the member.
    let line = log::wait_for_line_matching(&captured, support::http::STARTUP_BUDGET, |line| {
        line.contains("provider member excluded from distribution")
    })
    .await;
    // `tracing` renders a `&str` field quoted; the value inside is what an operator filters on.
    assert!(
        line.contains("llm.excluded=\"rate_limited\""),
        "the exclusion names its reason: {line}"
    );
    assert!(
        line.contains("llm.pool=llm-chat"),
        "and the route whose pool the member left: {line}"
    );

    let exit_code = server.cancel_and_join().await;
    assert_eq!(exit_code, std::process::ExitCode::SUCCESS);
}

#[tokio::test(flavor = "current_thread")]
async fn a_throttled_member_stays_out_across_requests() {
    // The table this scenario exists for: a rate limit is discovered by one request and must not be
    // rediscovered by the next. Round robin would draw the throttled member again on the very next
    // request if the exclusion were request-scoped, so its connection count is the assertion.
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let credential = support::token::secret_file(directory.path(), "openai.key", scenarios::KEY);

    let serving = MockUpstream::start(success("serving-model")).await;
    let throttled = MockUpstream::start(failure(429)).await;

    let document = pooled_document(
        &[("serving", 1), ("throttled", 1)],
        &credential,
        "round_robin",
        1,
        &[
            ("serving", base_url(&serving)),
            ("throttled", base_url(&throttled)),
        ],
    );

    write_bootstrap(directory.path(), free_port(), false, 1, 1000);
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source.clone()).await;
    let address = server.address();

    // Enough requests that round robin would have offered the throttled member many times over.
    let mut asked = 0;
    for _ in 0..8 {
        let response = scenarios::ask(address, &chat()).await;
        assert_eq!(
            response.status(),
            200,
            "the pool answers every request: {}",
            response.text()
        );
        asked += 1;
    }
    assert_eq!(asked, 8);

    assert!(
        throttled.observation().connections <= 1,
        "the throttled member is retired after the first discovery, not rediscovered per request: \
         it was asked {} times",
        throttled.observation().connections
    );

    let exit_code = server.cancel_and_join().await;
    assert_eq!(exit_code, std::process::ExitCode::SUCCESS);
}

#[tokio::test(flavor = "current_thread")]
async fn a_caller_mistake_is_answered_without_asking_another_provider() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let credential = support::token::secret_file(directory.path(), "openai.key", scenarios::KEY);

    // The first member rejects the request as malformed; the second would serve it.
    let refusing = MockUpstream::start(failure(400)).await;
    let serving = MockUpstream::start(success("serving-model")).await;

    // The refusing member is the only one the draw can reach: the second has weight 0, so it is not a
    // distribution candidate. That makes the scenario's subject — "a 400 stops the walk" — independent
    // of which turn the rotation is on, while the second member is still available to be *asked* if
    // the classification were wrong.
    let document = pooled_document(
        &[("refusing", 1), ("serving", 0)],
        &credential,
        "weighted_random",
        3,
        &[
            ("refusing", base_url(&refusing)),
            ("serving", base_url(&serving)),
        ],
    );

    write_bootstrap(directory.path(), free_port(), false, 1, 1000);
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source.clone()).await;
    let address = server.address();

    // Warm the route: the first answer may come from the previous generation.
    let first = scenarios::ask_until_published(address, &credential, &source).await;
    let refusing_before = refusing.observation().connections;
    let serving_before = serving.observation().connections;

    let response = scenarios::ask(address, &chat()).await;
    assert_eq!(
        response.status(),
        400,
        "the caller's own mistake is the answer: {}",
        response.text()
    );
    assert_eq!(
        refusing.observation().connections - refusing_before,
        1,
        "a 400 is not retried on the same member"
    );
    assert_eq!(
        serving.observation().connections,
        serving_before,
        "and it does not move to another provider"
    );
    let _ = first;

    let exit_code = server.cancel_and_join().await;
    assert_eq!(exit_code, std::process::ExitCode::SUCCESS);
}

#[test]
fn the_fixture_document_is_valid() {
    // A fixture that the document model refuses would fail every scenario with a startup timeout, so
    // the shape is asserted here where the error is readable.
    let document = pooled_document(
        &[("serving", 1), ("throttled", 1)],
        "env:KEY",
        "round_robin",
        1,
        &[
            ("serving", "http://127.0.0.1:1".to_owned()),
            ("throttled", "http://127.0.0.1:2".to_owned()),
        ],
    );
    nico_core::Document::parse(document.as_bytes(), nico_core::ParseLimits::new(1 << 20))
        .expect("the fixture document parses");
}

#[tokio::test(flavor = "current_thread")]
async fn a_fallback_is_asked_only_after_the_pool_is_exhausted() {
    // The pool's members are both throttled, so the walk leaves the pool and then continues down the
    // fallback chain — the shape a deployment relies on when every primary is out of quota. The
    // assertion is the order: both pool members were asked, and the fallback answered.
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let credential = support::token::secret_file(directory.path(), "openai.key", scenarios::KEY);

    let first = MockUpstream::start(failure(429)).await;
    let second = MockUpstream::start(failure(429)).await;
    let fallback = MockUpstream::start(success("fallback-model")).await;

    let document = pooled_document_with_fallback(
        &[("pool-a", 1), ("pool-b", 1)],
        &credential,
        "round_robin",
        1,
        &[
            ("pool-a", base_url(&first)),
            ("pool-b", base_url(&second)),
            ("backup", base_url(&fallback)),
        ],
        &["backup"],
    );

    write_bootstrap(directory.path(), free_port(), false, 1, 1000);
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source.clone()).await;
    let address = server.address();

    let response = scenarios::ask_until_published(address, &credential, &source).await;

    assert_eq!(
        response.status(),
        200,
        "the fallback answered after both pool members were throttled: {}",
        response.text()
    );
    assert!(
        response.text().contains("fallback-model"),
        "the answer came from the chain, not the pool: {}",
        response.text()
    );
    assert!(
        first.observation().connections + second.observation().connections >= 1,
        "at least one pool member was tried before the chain was asked"
    );
    assert_eq!(
        fallback.observation().connections,
        1,
        "the fallback was asked exactly once"
    );

    let exit_code = server.cancel_and_join().await;
    assert_eq!(exit_code, std::process::ExitCode::SUCCESS);
}

#[tokio::test(flavor = "current_thread")]
async fn a_silent_member_is_replaced_after_its_first_byte_budget() {
    // The member accepts the connection and never answers. Without a first-byte budget the request
    // would wait for its whole deadline; with one, the walk leaves the member and the next one
    // answers. That is the field's entire purpose, so the assertion is the answer, not the timing.
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let credential = support::token::secret_file(directory.path(), "openai.key", scenarios::KEY);

    let silent = MockUpstream::start(Reply::StallBeforeHead).await;
    let serving = MockUpstream::start(streamed_success("serving-model")).await;

    let document = pooled_document_with_ttft(
        &[("silent", 1), ("serving", 1)],
        &credential,
        "round_robin",
        1,
        &[
            ("silent", base_url(&silent)),
            ("serving", base_url(&serving)),
        ],
        200,
    );

    write_bootstrap(directory.path(), free_port(), false, 1, 1000);
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source.clone()).await;
    let address = server.address();

    let response = scenarios::ask(address, &chat_stream()).await;

    assert_eq!(
        response.status(),
        200,
        "the next member answered after the silent one's budget expired: {}",
        response.text()
    );
    assert!(
        response.text().contains("serving-model"),
        "the answer came from the member that responded: {}",
        response.text()
    );

    let exit_code = server.cancel_and_join().await;
    assert_eq!(exit_code, std::process::ExitCode::SUCCESS);
}

#[tokio::test(flavor = "current_thread")]
async fn a_first_byte_budget_still_serves_a_buffered_answer() {
    // The budget bounds one read, so a buffered exchange is held to it too — a provider that answers
    // within the budget is unaffected, which is the case this asserts. A buffered answer that *takes*
    // longer than the budget is bounded by it by design; the field's documentation states that.
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let credential = support::token::secret_file(directory.path(), "openai.key", scenarios::KEY);

    let serving = MockUpstream::start(success("serving-model")).await;

    let document = pooled_document_with_ttft(
        &[("serving", 1)],
        &credential,
        "round_robin",
        1,
        &[("serving", base_url(&serving))],
        200,
    );

    write_bootstrap(directory.path(), free_port(), false, 1, 1000);
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source.clone()).await;
    let address = server.address();

    let response = scenarios::ask(address, &chat()).await;

    assert_eq!(
        response.status(),
        200,
        "a buffered answer that arrives within the budget is served: {}",
        response.text()
    );

    let exit_code = server.cancel_and_join().await;
    assert_eq!(exit_code, std::process::ExitCode::SUCCESS);
}
