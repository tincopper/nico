//! The transitional adapter: a five-phase [`Plugin`] driven as the two-stage filter the hooks run.
//!
//! CONTEXT: temporary. [`Plugin`] is the contract, and [`HttpFilter`] is the seam today's hooks know.
//! This type is the single place the two meet, so a plugin can be written against the five phases
//! while the data plane still drives two of them. The five-phase hook rewrite (step 5b) deletes this
//! module together with [`HttpFilter`]; nothing else should construct it, and no new behaviour may
//! depend on it.
//!
//! Two boundaries are visible here on purpose:
//!
//! - **A refusal keeps its type.** [`PluginError`](super::PluginError) becomes an
//!   [`HttpError`] through the one conversion, so a refusal stays a typed `4xx` with its compiled
//!   presentation rather than becoming the adapter's own failure.
//! - **A synthesised response is not wired here.** [`Outcome::Respond`] needs the writer that
//!   `write_respond` owns, and the two-stage seam has no place to put a body. It is reported as an
//!   internal failure that names this boundary instead of being silently dropped: a plugin that
//!   answered and got a forwarded request would be a wrong answer, not a missing feature.

use std::{io, sync::Arc};

use async_trait::async_trait;
use pingora::http::{RequestHeader, ResponseHeader};

use super::{
    Outcome, Plugin,
    context::{ContextState, Phase},
};
use crate::{context::HttpContext, error::HttpError, filter::HttpFilter};

/// Drives one [`Plugin`] as an [`HttpFilter`], mapping the two header stages.
///
/// The instance name and the validated configuration are copied in once, when the plan is compiled:
/// a stage then costs a refcount bump per call instead of an allocation.
pub struct PluginFilter {
    plugin: Arc<dyn Plugin>,
    /// The instance name, kept as one `Arc<str>` because every stage hands it to the context.
    instance: Arc<str>,
    /// The kind's configuration as JSON text, never re-parsed on the request path.
    config: Arc<str>,
}

impl PluginFilter {
    /// Builds the adapter over `plugin`, whose context reports `config` as its configuration.
    #[must_use]
    pub fn new(plugin: Arc<dyn Plugin>, config: Arc<str>) -> Self {
        let instance: Arc<str> = Arc::from(plugin.name());
        Self {
            plugin,
            instance,
            config,
        }
    }

    /// Returns the failure that reports an outcome this seam cannot carry.
    ///
    /// It is [`HttpError::Internal`] with the boundary named in its cause, so a log reader sees
    /// "the two-stage adapter cannot write a response" rather than a plugin blamed for a
    /// limitation of the caller.
    fn unwired() -> HttpError {
        HttpError::Internal {
            source: Box::new(io::Error::other(
                "a plugin returned a synthesised response; the two-stage adapter cannot write one",
            )),
        }
    }
}

#[async_trait]
impl HttpFilter for PluginFilter {
    fn name(&self) -> &str {
        &self.instance
    }

    async fn on_request(
        &self,
        ctx: &mut HttpContext,
        request: &mut RequestHeader,
    ) -> Result<(), HttpError> {
        let mut state = ContextState::new(
            Phase::Request,
            ctx,
            request,
            Arc::clone(&self.instance),
            Arc::clone(&self.config),
        );
        match self.plugin.on_request(&mut state).await {
            Ok(Outcome::Continue) => Ok(()),
            // `Respond` reaches the writer in step 5b; any later outcome is equally not writable here.
            Ok(_outcome) => Err(Self::unwired()),
            Err(error) => Err(error.into()),
        }
    }

    async fn on_response(
        &self,
        ctx: &mut HttpContext,
        response: &mut ResponseHeader,
    ) -> Result<(), HttpError> {
        let mut state = ContextState::for_response(
            ctx,
            response,
            Arc::clone(&self.instance),
            Arc::clone(&self.config),
        );
        match self.plugin.on_response(&mut state).await {
            Ok(Outcome::Continue) => Ok(()),
            Ok(_outcome) => Err(Self::unwired()),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests;
