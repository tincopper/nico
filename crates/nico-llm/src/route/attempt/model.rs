//! The model a request names, and what a route's pool may do about it.
//!
//! The data plane chooses a target **before** the client's body arrives, so a pool cannot be narrowed
//! by the model a request will name — that fact only exists once the body is decoded. What can be
//! decided, and is decided here, is what happens when the naming and the pool do not agree.
//!
//! Three pieces:
//!
//! - the **logical model** the client asked for, kept beside the decoded canonical request so the
//!   pool's filter reads it instead of re-deriving it;
//! - **which members can serve it**: each member resolves the logical name through *its own*
//!   `model_map` and checks *its own* `model_allowlist`, because two providers may map the same
//!   logical name differently;
//! - the **refusal** a route gives when no member can, worded exactly like the one the encoding path
//!   produces so a caller cannot tell which stage noticed.

use super::{AttemptSequence, SelectionRefused};
use crate::schema::{CanonicalError, ModelId};

impl AttemptSequence {
    /// Returns the logical model the client asked for, once its body was decoded.
    pub(super) fn requested_model(&self) -> Option<&ModelId> {
        self.requested_model.as_ref()
    }

    /// Decides what a route does when no pool member can serve the requested model.
    ///
    /// Called once the body is decoded, before anything is encoded for the member in flight. The
    /// upstream head for this round was already written from a target chosen before the body arrived,
    /// so a mismatch discovered here cannot move the request to another member: pingora's hook order
    /// fixes the target first and offers the body afterwards. What this call decides is therefore the
    /// *answer*: when no member can serve the model, the client gets `403 model_not_allowed` instead
    /// of a member failure, and no attempt is encoded.
    ///
    /// A member that can serve the model is untouched, which is every single-member route and every
    /// pool whose members declare the same allowlist — a shape publication already requires.
    ///
    /// # Errors
    /// Returns [`SelectionRefused`] carrying `403 model_not_allowed` when no member can serve the
    /// model.
    pub(crate) fn refuse_unservable_model(&self) -> Result<(), SelectionRefused> {
        let Some(pool) = self.chain.pool() else {
            return Ok(());
        };
        // A member outside the pool is a fallback target: it is only reached after the pool has had
        // its attempts, so its own allowlist decides there rather than here.
        if !(0..pool.len()).any(|index| self.serves_model(index)) {
            return Err(SelectionRefused(Box::new(Self::model_refusal())));
        }
        Ok(())
    }

    /// Reports whether the member at `index` can serve the request's model.
    pub(super) fn serves_model(&self, index: usize) -> bool {
        let Some(logical) = self.requested_model() else {
            return true;
        };
        let route = self.chain.members()[index].exchange().route();
        let physical = route.model(logical).physical().clone();
        route.allows(&physical)
    }

    /// The canonical answer for "this route does not serve the model".
    ///
    /// Word for word what `ExchangeError::ModelNotAllowed` renders, so a caller sees the same code
    /// whether the mismatch was found while choosing a member or while encoding for one.
    pub(super) fn model_refusal() -> CanonicalError {
        CanonicalError::from_static_parts(
            nico_core::ErrorCategory::Client,
            "model_not_allowed",
            "the route does not serve this model",
            403,
        )
    }
}
