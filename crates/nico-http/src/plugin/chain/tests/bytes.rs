//! Body rewriting, the request budget, framing declarations and diagnostic output.

use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

use bytes::Bytes;

use super::fixture::{Behaviour, Fake, NeverFinishes, chain, response};
use crate::{
    Plugin, PluginPhases,
    error::HttpError,
    plugin::{PluginChain, PluginEntry},
    test_support::{PluginFixture, request_to, snapshot},
};

#[tokio::test]
async fn an_empty_chain_accepts_every_stage_and_returns_the_bytes_unchanged() {
    let chain = PluginChain::empty();
    let mut fixture = PluginFixture::started();
    let mut response = response();

    assert!(
        chain
            .run_request(&mut fixture.http, &mut fixture.request)
            .await
            .expect("an empty chain accepts")
            .is_none()
    );
    chain
        .run_response(&mut fixture.http, &mut response)
        .await
        .expect("an empty chain accepts");
    assert_eq!(
        chain
            .run_request_body(
                &mut fixture.http,
                &mut fixture.request,
                b"chunk".to_vec(),
                false
            )
            .await
            .expect("an empty chain forwards the chunk"),
        b"chunk"
    );
    assert_eq!(
        chain
            .run_response_body(
                &mut fixture.http,
                Some(&response),
                Bytes::from_static(b"chunk"),
                false
            )
            .expect("an empty chain forwards the chunk"),
        Some(Bytes::from_static(b"chunk"))
    );
}

#[tokio::test]
async fn request_body_stages_see_and_replace_the_chunk_in_order() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let chain = chain(vec![
        Fake::new("upper", Behaviour::Rewrite(b"rewritten"), &calls),
        Fake::new("observer", Behaviour::Continue, &calls),
    ]);
    let mut fixture = PluginFixture::started();

    let forwarded = chain
        .run_request_body(
            &mut fixture.http,
            &mut fixture.request,
            b"original".to_vec(),
            true,
        )
        .await
        .expect("no plugin refuses");

    assert_eq!(forwarded, b"rewritten");
    assert_eq!(
        *calls.lock().expect("calls lock"),
        ["upper:request_body", "observer:request_body"]
    );
}

#[tokio::test]
async fn a_response_body_stage_that_empties_the_chunk_drops_it() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let chain = chain(vec![Fake::new("truncate", Behaviour::Rewrite(b""), &calls)]);
    let mut fixture = PluginFixture::started();
    let response = response();

    let forwarded = chain
        .run_response_body(
            &mut fixture.http,
            Some(&response),
            Bytes::from_static(b"secret"),
            true,
        )
        .expect("the plugin accepts");

    assert_eq!(
        forwarded, None,
        "an emptied chunk is dropped, not sent as the original bytes"
    );
}

#[tokio::test]
async fn logging_runs_in_chain_order_without_a_response() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let chain = chain(vec![
        Fake::new("first", Behaviour::Continue, &calls),
        Fake::new("second", Behaviour::Continue, &calls),
    ]);
    let mut fixture = PluginFixture::started();

    chain.run_logging(&mut fixture.http, None).await;

    assert_eq!(
        *calls.lock().expect("calls lock"),
        ["first:logging", "second:logging"]
    );
}

#[tokio::test(start_paused = true)]
async fn a_request_stage_that_outlives_the_budget_is_stopped_by_the_chain() {
    // The clock is paused, so the 30 s fixture budget elapses instantly instead of being slept
    // through: the assertion is about which failure comes back, not about waiting.
    let chain = PluginChain::new(
        vec![PluginEntry::new(
            Arc::new(NeverFinishes) as Arc<dyn Plugin>,
            Arc::from("{}"),
        )]
        .into(),
    );
    let mut fixture = PluginFixture::started();

    let error = chain
        .run_request(&mut fixture.http, &mut fixture.request)
        .await
        .expect_err("the budget, not the plugin, ends the stage");

    assert!(
        matches!(error, HttpError::DeadlineExceeded { .. }),
        "a plugin that awaits for ever must end as the request's deadline, got {error:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_response_stage_that_outlives_the_budget_is_stopped_by_the_chain() {
    let chain = PluginChain::new(
        vec![PluginEntry::new(
            Arc::new(NeverFinishes) as Arc<dyn Plugin>,
            Arc::from("{}"),
        )]
        .into(),
    );
    let mut fixture = PluginFixture::started();
    let mut response = response();

    let error = chain
        .run_response(&mut fixture.http, &mut response)
        .await
        .expect_err("the budget, not the plugin, ends the stage");

    assert!(matches!(error, HttpError::DeadlineExceeded { .. }));
}

#[test]
fn only_a_plugin_that_declares_the_response_body_makes_the_chain_rewrite() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let quiet = chain(vec![Fake::new("quiet", Behaviour::Continue, &calls)]);
    let writer = chain(vec![
        Fake::new("writer", Behaviour::Continue, &calls)
            .declaring(PluginPhases::REQUEST | PluginPhases::RESPONSE_BODY),
    ]);

    assert!(
        !quiet.rewrites_response_body(),
        "a request-only plugin cannot change how the response is framed"
    );
    assert!(
        writer.rewrites_response_body(),
        "the declaration is what tells the data plane to drop the upstream length"
    );
}

#[test]
fn the_chain_names_its_plugins_in_execution_order() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let chain = chain(vec![
        Fake::new("first", Behaviour::Continue, &calls),
        Fake::new("second", Behaviour::Continue, &calls),
    ]);

    assert_eq!(chain.len(), 2);
    assert!(!chain.is_empty());
    assert_eq!(format!("{chain:?}"), r#"["first", "second"]"#);
    assert_eq!(format!("{:?}", PluginChain::default()), "[]");
}

/// A chain built from a request header the fixture never matched: the deadline is the document's.
#[test]
fn a_request_that_never_bound_a_route_still_has_the_document_budget() {
    let snapshot = snapshot(crate::test_support::PLUGIN_ROUTE);
    let request = request_to("example.test", "/api/items");
    let now = Instant::now();
    let mut ctx = crate::HttpContext::default();
    ctx.begin(&snapshot, &request, now);

    assert_eq!(
        ctx.deadline(),
        Some(now + std::time::Duration::from_millis(30_000)),
        "a global plugin runs before routing and is still bounded"
    );
    assert!(ctx.route_id().is_none());
}
