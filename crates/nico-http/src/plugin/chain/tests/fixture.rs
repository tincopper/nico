//! Fakes shared by the chain's test modules.
//!
//! A fake records the stage it ran in and applies one behaviour, so a test can assert the driver's
//! order and short-circuiting without standing up a session.

use std::sync::{Arc, Mutex};

use nico_core::ErrorCategory;
use pingora::http::{ResponseHeader, StatusCode};

use crate::{
    BodyTemplate, Plugin, PluginContext, PluginError, PluginInfo, PluginPhases, PluginResult,
    Rejection,
    plugin::{Outcome, PluginChain, PluginEntry},
};

/// What a fake plugin does in every stage it is asked about.
#[derive(Clone, Copy)]
pub(super) enum Behaviour {
    /// Continue, leaving the request alone.
    Continue,
    /// Refuse with the typed refusal of a missing credential.
    Refuse,
    /// Answer with a complete response.
    Answer,
    /// Replace the body a body stage was handed with these bytes.
    Rewrite(&'static [u8]),
}

/// A plugin that records the stage it ran in and applies one behaviour.
pub(super) struct Fake {
    name: &'static str,
    behaviour: Behaviour,
    phases: PluginPhases,
    calls: Arc<Mutex<Vec<String>>>,
}

impl Fake {
    pub(super) fn new(
        name: &'static str,
        behaviour: Behaviour,
        calls: &Arc<Mutex<Vec<String>>>,
    ) -> Self {
        Self {
            name,
            behaviour,
            phases: PluginPhases::REQUEST,
            calls: Arc::clone(calls),
        }
    }

    pub(super) fn declaring(mut self, phases: PluginPhases) -> Self {
        self.phases = phases;
        self
    }

    fn step(&self, stage: &str) {
        self.calls
            .lock()
            .expect("calls lock")
            .push(format!("{}:{stage}", self.name));
    }

    fn outcome(&self) -> PluginResult {
        match self.behaviour {
            Behaviour::Continue => Ok(Outcome::Continue),
            Behaviour::Refuse => Err(PluginError::Refused {
                rejection: rejection(),
                detail: "the request carries no credential",
                retry_after: None,
            }),
            Behaviour::Answer => Ok(Outcome::Respond(
                // `ResponseData::new` validates, so the fixture is a response the writer can send.
                crate::ResponseData::new(204, Vec::new(), Vec::new())
                    .expect("the fixture is a status"),
            )),
            Behaviour::Rewrite(_) => Ok(Outcome::Continue),
        }
    }
}

#[async_trait::async_trait]
impl Plugin for Fake {
    fn name(&self) -> &str {
        self.name
    }

    fn info(&self) -> PluginInfo {
        PluginInfo::new("0.0.1", "a fake", "{}").with_phases(self.phases)
    }

    async fn on_request(&self, _ctx: &mut dyn PluginContext) -> PluginResult {
        self.step("request");
        self.outcome()
    }

    async fn on_request_body(&self, ctx: &mut dyn PluginContext) -> PluginResult {
        self.step("request_body");
        if let Behaviour::Rewrite(bytes) = self.behaviour {
            ctx.set_request_body(bytes.to_vec())?;
        }
        self.outcome()
    }

    async fn on_response(&self, _ctx: &mut dyn PluginContext) -> PluginResult {
        self.step("response");
        self.outcome()
    }

    fn on_response_body(&self, ctx: &mut dyn PluginContext) -> PluginResult {
        self.step("response_body");
        if let Behaviour::Rewrite(bytes) = self.behaviour {
            ctx.set_response_body(bytes.to_vec())?;
        }
        self.outcome()
    }

    async fn on_logging(&self, _ctx: &mut dyn PluginContext) {
        self.step("logging");
    }
}

/// The refusal every [`Behaviour::Refuse`] produces.
pub(super) fn rejection() -> Arc<Rejection> {
    Arc::new(
        Rejection::new(
            StatusCode::UNAUTHORIZED,
            "auth_missing",
            "the request carries no credential",
            "application/json",
            ErrorCategory::Client,
            BodyTemplate::error_envelope(),
        )
        .expect("the fixture rejection is valid"),
    )
}

/// Builds a chain over the given fakes.
pub(super) fn chain(fakes: Vec<Fake>) -> PluginChain {
    PluginChain::new(
        fakes
            .into_iter()
            .map(|fake| PluginEntry::new(Arc::new(fake) as Arc<dyn Plugin>, Arc::from("{}")))
            .collect::<Vec<_>>()
            .into(),
    )
}

/// A plugin that never returns, so only the request's budget can end the stage.
pub(super) struct NeverFinishes;

#[async_trait::async_trait]
impl Plugin for NeverFinishes {
    fn name(&self) -> &str {
        "never-finishes"
    }

    fn info(&self) -> PluginInfo {
        PluginInfo::new("0.0.1", "never returns", "{}")
    }

    async fn on_request(&self, _ctx: &mut dyn PluginContext) -> PluginResult {
        std::future::pending().await
    }

    async fn on_response(&self, _ctx: &mut dyn PluginContext) -> PluginResult {
        std::future::pending().await
    }
}

/// Builds a response header with the status a response stage runs against.
pub(super) fn response() -> ResponseHeader {
    ResponseHeader::build(StatusCode::OK, None).expect("response builds")
}
