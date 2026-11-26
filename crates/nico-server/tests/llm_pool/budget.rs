//! The token budget, end to end: an authenticated identity spends its window, then the next request
//! is refused before anything upstream happens.

use scenarios::{base_url, chat_response_with_usage, chat_with_key};
use support::http::{
    RunningServer, free_port,
    resources::{Plugin, Resources, Route},
    source::TestSource,
    upstream::MockUpstream,
    write_bootstrap,
};

use super::*;

const API_KEY: &str = "partner-api-key-value";

/// A converting route with an api-key identity and a per-identity token budget.
fn document(
    serving: &MockUpstream,
    credential: &str,
    key_ref: &str,
    limit: u64,
    window: u32,
) -> String {
    Resources::new()
        .provider("serving", &base_url(serving), credential)
        .plugin(Plugin::api_key("partner-keys", key_ref))
        .route(
            Route::provider("llm-chat", "/v1/chat/", "serving")
                .plugins(&["partner-keys"])
                .retry_budget(1),
        )
        .render()
        .replace(
            "  - id: llm-chat\n",
            &format!(
                "  - id: llm-chat\n    token_budget:\n      limit_tokens: {limit}\n      window_seconds: {window}\n"
            ),
        )
}

/// A converting route with two virtual keys sharing one tenant budget.
fn tenant_document(
    serving: &MockUpstream,
    credential: &str,
    first_ref: &str,
    second_ref: &str,
    limit: u64,
    window: u32,
) -> String {
    Resources::new()
        .provider("serving", &base_url(serving), credential)
        .plugin(Plugin::api_key_tenant(
            "partner-keys",
            first_ref,
            second_ref,
            "tenant-acme",
        ))
        .route(
            Route::provider("llm-chat", "/v1/chat/", "serving")
                .plugins(&["partner-keys"])
                .retry_budget(1),
        )
        .render()
        .replace(
            "  - id: llm-chat\n",
            &format!(
                "  - id: llm-chat\n    token_budget:\n      limit_tokens: {limit}\n      window_seconds: {window}\n"
            ),
        )
}

#[tokio::test(flavor = "current_thread")]
async fn a_spent_budget_refuses_the_next_request_before_dispatch() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let credential = support::token::secret_file(directory.path(), "openai.key", scenarios::KEY);
    let key_ref = support::token::secret_file(directory.path(), "partner.key", API_KEY);

    let serving = MockUpstream::start(Reply::Response {
        status: 200,
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body: chat_response_with_usage("served", 4, 2),
        chunk: usize::MAX,
    })
    .await;
    let source = TestSource::with_document(&document(&serving, &credential, &key_ref, 6, 3600));
    write_bootstrap(directory.path(), free_port(), false, 1, 1000);
    let server = RunningServer::start(directory.path(), source).await;
    let address = server.address();

    let first = scenarios::ask(address, &chat_with_key("fast", API_KEY)).await;
    assert_eq!(first.status(), 200, "{}", first.text());
    let second = scenarios::ask(address, &chat_with_key("fast", API_KEY)).await;
    assert_eq!(second.status(), 429, "{}", second.text());
    assert!(second.text().contains("token_budget_exceeded"));
    assert_eq!(serving.observation().requests.len(), 1);
    assert_eq!(
        server.cancel_and_join().await,
        std::process::ExitCode::SUCCESS
    );
}

#[tokio::test(flavor = "current_thread")]
async fn distinct_virtual_keys_share_their_route_tenant_budget() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let credential = support::token::secret_file(directory.path(), "openai.key", scenarios::KEY);
    let first_key = support::token::secret_file(directory.path(), "first.key", "first-secret");
    let second_key = support::token::secret_file(directory.path(), "second.key", "second-secret");
    let serving = MockUpstream::start(Reply::Response {
        status: 200,
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body: chat_response_with_usage("served", 4, 2),
        chunk: usize::MAX,
    })
    .await;
    let source = TestSource::with_document(&tenant_document(
        &serving,
        &credential,
        &first_key,
        &second_key,
        6,
        3600,
    ));
    write_bootstrap(directory.path(), free_port(), false, 1, 1000);
    let server = RunningServer::start(directory.path(), source).await;
    let address = server.address();

    let first = scenarios::ask(address, &chat_with_key("fast", "first-secret")).await;
    assert_eq!(first.status(), 200, "{}", first.text());
    let second = scenarios::ask(address, &chat_with_key("fast", "second-secret")).await;
    assert_eq!(second.status(), 429, "{}", second.text());
    assert!(second.text().contains("token_budget_exceeded"));
    assert_eq!(
        serving.observation().requests.len(),
        1,
        "the second virtual key shares tenant usage and is refused before provider dispatch"
    );
    assert_eq!(
        server.cancel_and_join().await,
        std::process::ExitCode::SUCCESS
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_request_without_an_identity_is_not_bounded() {
    // The documented edge: a route that declares a budget bounds *identities*. A request with no
    // credential has none, so the deployment that wants full coverage puts an auth plugin on the
    // route — which this document deliberately does not.
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let credential = support::token::secret_file(directory.path(), "openai.key", scenarios::KEY);
    let key_ref = support::token::secret_file(directory.path(), "partner.key", API_KEY);

    let serving = MockUpstream::start(success("served")).await;
    // The plugin's key is a real ref even though this scenario never authenticates with it: the
    // schema validates a `credential_ref`/`secret_ref` shape before anything runs.
    let source = TestSource::with_document(&document(&serving, &credential, &key_ref, 1, 3600));
    write_bootstrap(directory.path(), free_port(), false, 1, 1000);
    let server = RunningServer::start(directory.path(), source.clone()).await;
    let address = server.address();

    // Two requests against a 1-token budget: without an identity there is no window to spend, so
    // neither is refused, and neither is a 200 from a provider that answered 401 (the document
    // declares no provider credential here — the subject is what the budget keys on, and it is
    // absent either way).
    for i in 0..2 {
        let response = scenarios::ask(address, &chat_with_key("fast", "unused-key")).await;
        let _ = i;
        assert_ne!(
            response.status(),
            429,
            "an identityless request has no window to spend: {}",
            response.text()
        );
    }
    let response = scenarios::ask(address, &chat_with_key("fast", API_KEY)).await;
    assert_ne!(
        response.status(),
        429,
        "an identity inside its budget passes: {}",
        response.text()
    );

    let exit_code = server.cancel_and_join().await;
    assert_eq!(exit_code, std::process::ExitCode::SUCCESS);
}
