//! Model filtering and the pool it does not fit in.
//!
//! The data plane chooses a target before the client's body arrives, so the model a request names
//! cannot narrow the member list first. Two consequences, both asserted here:
//!
//! 1. a pool whose members serve **different** model sets is refused at publication — the shape has
//!    no correct request-time answer, so the document never reaches service;
//! 2. a pool whose members agree keeps working, and a model none of them allows is answered by the
//!    member's own `403 model_not_allowed` — the check inside one member is unchanged.

use scenarios::{base_url, chat_with_model, success};
use support::http::resources::{Resources, Route};

use super::*;

/// A pool over `members`, each restricted to the same `allowed` set.
fn uniform_document(
    members: &[(&str, &MockUpstream)],
    credential: &str,
    allowed: &[&str],
) -> String {
    let mut resources = Resources::new();
    for (name, upstream) in members {
        resources =
            resources.provider_with_allowlist(name, &base_url(upstream), credential, allowed);
    }
    let pooled: Vec<(&str, u16)> = members.iter().map(|(name, _)| (*name, 1)).collect();
    resources
        .route(
            Route::provider("llm-chat", "/v1/chat/", pooled[0].0)
                .pool(&pooled, "round_robin")
                .retry_budget(1),
        )
        .render()
}

/// A pool whose members serve disjoint model sets.
fn disjoint_document(only_a: &MockUpstream, only_b: &MockUpstream, credential: &str) -> String {
    Resources::new()
        .provider_with_allowlist("only-a", &base_url(only_a), credential, &["model-a"])
        .provider_with_allowlist("only-b", &base_url(only_b), credential, &["model-b"])
        .route(
            Route::provider("llm-chat", "/v1/chat/", "only-a")
                .pool(&[("only-a", 1), ("only-b", 1)], "round_robin")
                .retry_budget(1),
        )
        .render()
}

#[tokio::test(flavor = "current_thread")]
async fn a_pool_whose_members_serve_different_models_is_refused_at_publication() {
    // The gateway picks a member before it knows which model the request names, so a pool with
    // disagreeing members could route a request to a provider that must refuse it while another
    // member would have served it. The document is refused instead, and the process keeps serving
    // whatever it was serving before.
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let credential = support::token::secret_file(directory.path(), "openai.key", scenarios::KEY);

    let only_a = MockUpstream::start(success("a-model")).await;
    let only_b = MockUpstream::start(success("b-model")).await;

    // A valid document first, so the refusal is observable as "the previous generation kept serving"
    // rather than as "the process never started".
    let healthy = MockUpstream::start(success("serving-model")).await;
    let source = TestSource::with_document(&uniform_document(
        &[("serving", &healthy)],
        &credential,
        &["model-a", "model-b"],
    ));
    write_bootstrap(directory.path(), free_port(), false, 1, 1000);
    let server = RunningServer::start(directory.path(), source.clone()).await;
    let address = server.address();

    // Warm the route so the reload below has a generation to keep.
    let _ = scenarios::ask_until_published(address, &credential, &source).await;

    source.publish(&disjoint_document(&only_a, &only_b, &credential));

    // The refused document changes nothing: the healthy member still answers.
    let response = scenarios::ask(address, &chat_with_model("model-a")).await;
    assert_eq!(
        response.status(),
        200,
        "the previously published generation keeps serving: {}",
        response.text()
    );
    assert!(
        response.text().contains("serving-model"),
        "the answer came from the generation that was already in service: {}",
        response.text()
    );
    assert_eq!(
        only_a.observation().connections + only_b.observation().connections,
        0,
        "no member of the refused pool ever took traffic"
    );

    let exit_code = server.cancel_and_join().await;
    assert_eq!(exit_code, std::process::ExitCode::SUCCESS);
}

#[tokio::test(flavor = "current_thread")]
async fn a_pool_whose_members_agree_serves_the_models_they_allow() {
    // The shape that survives: both members allow the same set, so the choice between them needs no
    // model to be known and the request is distributed as usual.
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let credential = support::token::secret_file(directory.path(), "openai.key", scenarios::KEY);

    let one = MockUpstream::start(success("one-model")).await;
    let two = MockUpstream::start(success("two-model")).await;

    let source = TestSource::with_document(&uniform_document(
        &[("one", &one), ("two", &two)],
        &credential,
        &["model-a", "model-b"],
    ));
    write_bootstrap(directory.path(), free_port(), false, 1, 1000);
    let server = RunningServer::start(directory.path(), source.clone()).await;
    let address = server.address();

    for _ in 0..2 {
        let response = scenarios::ask(address, &chat_with_model("model-b")).await;
        assert_eq!(response.status(), 200, "{}", response.text());
    }

    assert_eq!(
        one.observation().connections + two.observation().connections,
        2,
        "both members served a request, so the pool still distributes"
    );

    let exit_code = server.cancel_and_join().await;
    assert_eq!(exit_code, std::process::ExitCode::SUCCESS);
}

#[tokio::test(flavor = "current_thread")]
async fn a_model_the_pool_does_not_allow_is_refused_by_the_member() {
    // The check inside a member is unchanged: it answers `403 model_not_allowed` for a model its own
    // allowlist does not name, and that answer reaches the caller.
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let credential = support::token::secret_file(directory.path(), "openai.key", scenarios::KEY);

    let serving = MockUpstream::start(success("serving-model")).await;

    let source = TestSource::with_document(&uniform_document(
        &[("serving", &serving)],
        &credential,
        &["model-a"],
    ));
    write_bootstrap(directory.path(), free_port(), false, 1, 1000);
    let server = RunningServer::start(directory.path(), source.clone()).await;
    let address = server.address();

    let _ = scenarios::ask_until_published(address, &credential, &source).await;

    let response = scenarios::ask(address, &chat_with_model("model-z")).await;

    assert_eq!(response.status(), 403, "{}", response.text());
    assert!(
        response.text().contains("model_not_allowed"),
        "the member's own allowlist answered: {}",
        response.text()
    );
    // The upstream head is written before the body is encoded, so the provider does see a request
    // line. What it must never see is the body: the refusal happens while the canonical request is
    // being encoded for a member whose allowlist does not name the model.
    assert!(
        serving
            .observation()
            .requests
            .iter()
            .all(|request| request.body_len() == 0),
        "the provider never received a body for a model the route does not allow: {}",
        serving.observation().summary()
    );

    let exit_code = server.cancel_and_join().await;
    assert_eq!(exit_code, std::process::ExitCode::SUCCESS);
}
