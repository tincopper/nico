//! The token meter: what a successful converted answer reports, and what it must not.
//!
//! Every scenario here needs the self-reporting address, so they share the metrics bootstrap and
//! the `scenarios` fixtures with the pool file. The counters are process-level: each scenario uses
//! its own `model_map` value so one case's samples cannot be read as another's.

use scenarios::{chat_response_with_usage, pooled_document_with_model_map};

use super::*;

#[tokio::test(flavor = "current_thread")]
async fn a_converted_answer_reports_the_providers_token_counts() {
    // The provider's own numbers, not a gateway measurement: the answer says 11 prompt and 7
    // completion tokens, and the metric must carry exactly those under the physical model name.
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let credential = support::token::secret_file(directory.path(), "openai.key", scenarios::KEY);

    let serving = MockUpstream::start(Reply::Response {
        status: 200,
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body: chat_response_with_usage("physical-tokens", 11, 7),
        chunk: usize::MAX,
    })
    .await;

    // The document maps the caller's `fast` to a physical name, so the metric's `model` label shows
    // the provider's name and not the caller's — the distinction a bill is made against.
    let document = pooled_document_with_model_map(
        &[("serving", 1)],
        &credential,
        &[("serving", base_url(&serving))],
        &[("fast", "physical-tokens")],
    );

    let port = free_port();
    let metrics_port = free_port();
    support::http::write_bootstrap_with_metrics(directory.path(), port, metrics_port);
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source.clone()).await;
    let address = server.address();

    let response = scenarios::ask_until_published(address, &credential, &source).await;
    assert_eq!(response.status(), 200, "{}", response.text());

    let metrics_address = server
        .metrics_address()
        .expect("the fixture declared a metrics section");
    let scrape = scenarios::fetch(metrics_address, "/metrics").await;

    for (kind, expected) in [("input", 11u64), ("output", 7)] {
        let line = scenarios::counter_line(
            &scrape,
            "gateway_llm_tokens_total",
            &[
                "route_id=\"llm-chat\"",
                "provider=\"serving\"",
                "model=\"physical-tokens\"",
                &format!("kind=\"{kind}\""),
            ],
        );
        assert!(
            line.ends_with(&format!(" {expected}")),
            "the {kind} count is what the provider reported: {line}"
        );
    }
    assert!(
        !scrape.contains("kind=\"cache_read\""),
        "an unreported kind produces no sample"
    );

    let exit_code = server.cancel_and_join().await;
    assert_eq!(exit_code, std::process::ExitCode::SUCCESS);
}

#[tokio::test(flavor = "current_thread")]
async fn only_the_attempt_that_served_is_counted() {
    // A pool request that meets a failing member first and succeeds on the second must count one
    // usage, not two: the failure produced no token report, and folding it in would bill the caller
    // for an answer they never received.
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let credential = support::token::secret_file(directory.path(), "openai.key", scenarios::KEY);

    let failing = MockUpstream::start(failure(502)).await;
    let serving = MockUpstream::start(Reply::Response {
        status: 200,
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body: chat_response_with_usage("physical-x", 13, 5),
        chunk: usize::MAX,
    })
    .await;

    let document = pooled_document_with_model_map(
        &[("failing", 1), ("serving", 1)],
        &credential,
        &[
            ("failing", base_url(&failing)),
            ("serving", base_url(&serving)),
        ],
        &[("fast", "physical-x")],
    );

    let port = free_port();
    let metrics_port = free_port();
    support::http::write_bootstrap_with_metrics(directory.path(), port, metrics_port);
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source.clone()).await;
    let address = server.address();

    // Two requests: the rotation puts the failing member first for one of them, so at least one
    // request walks both members and only the serving one may be counted.
    for _ in 0..2 {
        let response = scenarios::ask(address, &chat()).await;
        assert_eq!(response.status(), 200, "{}", response.text());
    }

    let metrics_address = server
        .metrics_address()
        .expect("the fixture declared a metrics section");
    // The counters are process-level and every scenario in this binary shares one server, so the
    // assertion is on the increment this scenario produced, not on an absolute total.
    let scrape = scenarios::fetch(metrics_address, "/metrics").await;
    let line = scenarios::counter_line(
        &scrape,
        "gateway_llm_tokens_total",
        &[
            "provider=\"serving\"",
            "model=\"physical-x\"",
            "kind=\"input\"",
        ],
    );
    let total: u64 = line
        .rsplit_once(' ')
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or_default();
    assert!(
        total >= 26,
        "both served answers were counted (13 each): {line}"
    );
    let token_lines: Vec<&str> = scrape
        .lines()
        .filter(|line| line.starts_with("gateway_llm_tokens_total"))
        .collect();
    assert!(
        token_lines
            .iter()
            .all(|line| !line.contains("provider=\"failing\"")),
        "the failed member reported nothing and must not appear in the token family: {token_lines:?}"
    );

    let exit_code = server.cancel_and_join().await;
    assert_eq!(exit_code, std::process::ExitCode::SUCCESS);
}
