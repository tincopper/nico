//! Tests for the transitional adapter: a refusal keeps its type, and an outcome it cannot carry is
//! reported rather than dropped.
//!
//! The subject is the seam, not a policy: which `HttpError` a plugin result becomes, and which stage
//! of the response the adapter hands over. A silent `Continue` for an outcome the two-stage data plane
//! cannot write would be the failure these tests exist to prevent.

use std::sync::Arc;

use async_trait::async_trait;
use nico_core::ErrorCategory;
use pingora::http::{RequestHeader, ResponseHeader, StatusCode};

use super::PluginFilter;
use crate::{
    context::HttpContext,
    error::{BodyTemplate, HttpError, Rejection},
    filter::HttpFilter,
    plugin::{Outcome, Plugin, PluginContext, PluginError, PluginInfo, PluginResult, ResponseData},
};

/// A plugin that refuses every request with a compiled `401`.
struct Refusing;

#[async_trait]
impl Plugin for Refusing {
    fn name(&self) -> &str {
        "refusing-instance"
    }

    fn info(&self) -> PluginInfo {
        PluginInfo::new("0.0.1", "refuses every request", "{}")
    }

    async fn on_request(&self, _ctx: &mut dyn PluginContext) -> PluginResult {
        Err(PluginError::Refused {
            rejection: rejection(),
            detail: "the fixture refuses the request",
            retry_after: None,
        })
    }
}

/// A plugin that answers the request instead of letting it be forwarded.
struct Answering;

#[async_trait]
impl Plugin for Answering {
    fn name(&self) -> &str {
        "answering-instance"
    }

    fn info(&self) -> PluginInfo {
        PluginInfo::new("0.0.1", "answers every request", "{}")
    }

    async fn on_request(&self, _ctx: &mut dyn PluginContext) -> PluginResult {
        let response =
            ResponseData::new(204, Vec::new(), Vec::new()).expect("the fixture response is valid");
        Ok(Outcome::Respond(response))
    }
}

/// A plugin that stamps a response header, so the adapter's response stage is observable.
struct HeaderStamp;

#[async_trait]
impl Plugin for HeaderStamp {
    fn name(&self) -> &str {
        "header-stamp"
    }

    fn info(&self) -> PluginInfo {
        PluginInfo::new("0.0.1", "stamps the response", "{}")
    }

    async fn on_response(&self, ctx: &mut dyn PluginContext) -> PluginResult {
        ctx.set_response_header("x-stage", "response")?;
        Ok(Outcome::Continue)
    }
}

/// A compiled authentication refusal, as a kind would build at publication.
fn rejection() -> Arc<Rejection> {
    Arc::new(
        Rejection::new(
            StatusCode::UNAUTHORIZED,
            "auth_invalid",
            "the credential is not acceptable",
            "application/json",
            ErrorCategory::Client,
            BodyTemplate::error_envelope(),
        )
        .expect("the fixture rejection is valid"),
    )
}

/// Returns an adapter over `plugin` with an empty configuration text.
fn adapter(plugin: Arc<dyn Plugin>) -> PluginFilter {
    PluginFilter::new(plugin, Arc::from("{}"))
}

/// Returns a request the adapter can be driven with.
fn request() -> RequestHeader {
    RequestHeader::build("GET", b"/api/items", None).expect("the fixture request builds")
}

#[tokio::test]
async fn a_refusal_keeps_its_compiled_presentation() {
    let filter = adapter(Arc::new(Refusing));
    let mut ctx = HttpContext::default();
    let mut request = request();

    let error = filter
        .on_request(&mut ctx, &mut request)
        .await
        .expect_err("the fixture plugin refuses every request");

    assert!(
        matches!(error, HttpError::Rejected { .. }),
        "a refusal must not become the adapter's own failure: {error:?}"
    );
    let respond = error.respond().expect("a refusal is renderable");
    assert_eq!(respond.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(respond.code(), "auth_invalid");
    assert_eq!(error.category(), ErrorCategory::Client);
}

#[tokio::test]
async fn a_synthesised_response_names_the_adapter_boundary() {
    let filter = adapter(Arc::new(Answering));
    let mut ctx = HttpContext::default();
    let mut request = request();

    let error = filter
        .on_request(&mut ctx, &mut request)
        .await
        .expect_err("the two-stage seam cannot write a synthesised response");

    assert!(
        matches!(error, HttpError::Internal { .. }),
        "an outcome the seam cannot carry is this side's defect, not the caller's: {error:?}"
    );
    let cause = std::error::Error::source(&error)
        .map(ToString::to_string)
        .unwrap_or_default();
    assert!(
        cause.contains("two-stage adapter"),
        "the cause must name the boundary so it is not blamed on the plugin: {cause}"
    );
}

#[tokio::test]
async fn the_response_stage_is_driven_with_the_response_attached() {
    let filter = adapter(Arc::new(HeaderStamp));
    let mut ctx = HttpContext::default();
    let mut response = ResponseHeader::build(StatusCode::OK, None).expect("the fixture builds");

    filter
        .on_response(&mut ctx, &mut response)
        .await
        .expect("the plugin only stamps a header");

    assert_eq!(
        response
            .headers
            .get("x-stage")
            .and_then(|v| v.to_str().ok()),
        Some("response"),
        "the adapter must hand the response over, not a fabricated request"
    );
}

#[test]
fn the_filter_reports_the_instance_name_the_document_wrote() {
    let filter = adapter(Arc::new(HeaderStamp));

    assert_eq!(filter.name(), "header-stamp");
}
