//! Order, short-circuiting, the `Respond` boundary and failure attribution.

use std::{
    error::Error as _,
    sync::{Arc, Mutex},
};

use bytes::Bytes;

use super::fixture::{Behaviour, Fake, chain, response};
use crate::{error::HttpError, test_support::PluginFixture};

#[tokio::test]
async fn request_stages_run_in_chain_order() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let chain = chain(vec![
        Fake::new("first", Behaviour::Continue, &calls),
        Fake::new("second", Behaviour::Continue, &calls),
    ]);
    let mut fixture = PluginFixture::started();

    let answered = chain
        .run_request(&mut fixture.http, &mut fixture.request)
        .await
        .expect("no plugin refuses");

    assert!(answered.is_none(), "nothing answered the request");
    assert_eq!(
        *calls.lock().expect("calls lock"),
        ["first:request", "second:request"]
    );
}

#[tokio::test]
async fn the_first_refusal_short_circuits_the_request_chain() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let chain = chain(vec![
        Fake::new("first", Behaviour::Continue, &calls),
        Fake::new("second", Behaviour::Refuse, &calls),
        Fake::new("third", Behaviour::Continue, &calls),
    ]);
    let mut fixture = PluginFixture::started();

    let error = chain
        .run_request(&mut fixture.http, &mut fixture.request)
        .await
        .expect_err("the second plugin refuses");

    assert!(
        matches!(&error, HttpError::Rejected { rejection, .. } if rejection.code() == "auth_missing"),
        "the refusal must keep the presentation it was compiled with, got {error:?}"
    );
    assert_eq!(
        *calls.lock().expect("calls lock"),
        ["first:request", "second:request"],
        "the third plugin must not run"
    );
}

#[tokio::test]
async fn a_synthesised_response_stops_the_request_chain_and_is_returned() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let chain = chain(vec![
        Fake::new("answers", Behaviour::Answer, &calls),
        Fake::new("after", Behaviour::Refuse, &calls),
    ]);
    let mut fixture = PluginFixture::started();

    let answered = chain
        .run_request(&mut fixture.http, &mut fixture.request)
        .await
        .expect("answering is not a failure");

    let data = answered.expect("the first plugin answered");
    assert_eq!(data.status(), 204);
    assert_eq!(
        *calls.lock().expect("calls lock"),
        ["answers:request"],
        "a plugin that answered must not be followed by one that refuses"
    );
}

#[tokio::test]
async fn a_synthesised_response_from_a_later_stage_is_an_internal_failure() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let chain = chain(vec![Fake::new("answers", Behaviour::Answer, &calls)]);
    let mut fixture = PluginFixture::started();
    let mut response = response();

    let error = chain
        .run_response(&mut fixture.http, &mut response)
        .await
        .expect_err("only the request stage may answer");

    assert!(matches!(error, HttpError::Internal { .. }), "got {error:?}");
    let cause = error.source().map(ToString::to_string).unwrap_or_default();
    assert!(
        cause.contains("only carried from the request stage"),
        "the cause must name the boundary: {cause}"
    );
}

#[tokio::test]
async fn a_failure_names_the_instance_that_produced_it() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let chain = chain(vec![
        Fake::new("first", Behaviour::Continue, &calls),
        Fake::new("edge-firewall", Behaviour::Refuse, &calls),
    ]);
    let mut fixture = PluginFixture::started();

    chain
        .run_request(&mut fixture.http, &mut fixture.request)
        .await
        .expect_err("the second plugin refuses");

    assert_eq!(
        fixture.http.failed_plugin(),
        Some("edge-firewall"),
        "an operator must be able to name the instance without parsing the response body"
    );
}

#[tokio::test]
async fn a_synthesised_response_from_the_request_body_stage_is_an_internal_failure() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let chain = chain(vec![Fake::new("answers", Behaviour::Answer, &calls)]);
    let mut fixture = PluginFixture::started();

    let error = chain
        .run_request_body(
            &mut fixture.http,
            &mut fixture.request,
            b"chunk".to_vec(),
            true,
        )
        .await
        .expect_err("only the request stage may answer");

    assert!(matches!(error, HttpError::Internal { .. }), "got {error:?}");
    assert_eq!(fixture.http.failed_plugin(), Some("answers"));
}

#[test]
fn a_synthesised_response_from_the_response_body_stage_is_an_internal_failure() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let chain = chain(vec![Fake::new("answers", Behaviour::Answer, &calls)]);
    let mut fixture = PluginFixture::started();
    let response = response();

    let error = chain
        .run_response_body(
            &mut fixture.http,
            Some(&response),
            Bytes::from_static(b"chunk"),
            true,
        )
        .expect_err("only the request stage may answer");

    assert!(matches!(error, HttpError::Internal { .. }), "got {error:?}");
    assert_eq!(fixture.http.failed_plugin(), Some("answers"));
}

#[tokio::test]
async fn response_stages_run_in_chain_order() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let chain = chain(vec![
        Fake::new("first", Behaviour::Continue, &calls),
        Fake::new("second", Behaviour::Continue, &calls),
    ]);
    let mut fixture = PluginFixture::started();
    let mut response = response();

    chain
        .run_response(&mut fixture.http, &mut response)
        .await
        .expect("no plugin refuses");

    assert_eq!(
        *calls.lock().expect("calls lock"),
        ["first:response", "second:response"]
    );
}
