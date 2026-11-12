//! What one failed attempt means for the pool: retire the member briefly, move on, or stop.
//!
//! The classification comes **before** any counter moves, and that order is the whole design. A
//! failure's category decides three things at once — whether it consumes the route's retry budget,
//! whether the member is tried again this request, and whether the breaker hears about it — so a
//! classification made after the fact would let one number answer for three different events.
//!
//! The distinction this module exists for is between a failure **of the member** and a failure **of
//! the request**:
//!
//! - `400`/`404`/`422` are the caller's. Retrying them on another provider would multiply one billing
//!   event into several and return the same answer, so the attempt is final.
//! - `429` and `401`/`403` are about *this member right now* (a throttled window, a credential that
//!   stopped working). Another member may well succeed, so the walk moves on — but the route's retry
//!   budget is untouched, because the budget measures "how hard this route may try the same target",
//!   not "how many providers exist".
//! - Anything else that is retryable is the member failing to serve: the budget applies, and the
//!   breaker counts it.
//!
//! The exclusion table this module feeds is **not** the breaker table. It answers "should this member
//! take traffic for the next few seconds" — a member-scoped question with a category-chosen cooldown —
//! while the breaker answers "is this provider target healthy at all". Merging them would let one 429
//! open a breaker for the cooldown a real outage earns (see the task's design §6).

use std::time::Duration;

use crate::{route::exclusion::Reason, schema::CanonicalError};

/// What one failed attempt means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// The caller's request is the problem: answer with it, do not try another member.
    Final,
    /// This member should not take traffic for a while, and the request moves on.
    ///
    /// The cooldown travels with the decision because it is a property of the category, not of the
    /// deployment: a throttled member is worth retrying in seconds, a bad credential in minutes.
    Exclude {
        /// Why, for the log and the metric label.
        reason: Reason,
        /// How long the member stays out.
        cooldown: Duration,
    },
    /// The member failed to serve a request it was asked to serve: the retry budget applies, and the
    /// breaker counts it.
    Failed,
}

/// Classifies one provider answer against the member that gave it.
///
/// A 2xx never reaches here — the data plane reports success separately. `status` is the head's status,
/// and `error` is the canonical value the member's own closed table produced for it.
#[must_use]
pub fn classify(status: u16, error: &CanonicalError) -> Disposition {
    match status {
        // The caller's own mistake. Another member would answer the same, and asking it would bill the
        // request twice for one fault.
        400 | 404 | 422 => Disposition::Final,
        // This member's quota or credential, not this request: another member may serve it.
        429 => Disposition::Exclude {
            reason: Reason::RateLimited,
            cooldown: Reason::RateLimited.cooldown(),
        },
        401 | 403 => Disposition::Exclude {
            reason: Reason::Credential,
            cooldown: Reason::Credential.cooldown(),
        },
        // A member that answered but failed to serve. Whether the route tries it again is the budget's
        // question, asked by the caller.
        _ if error.retryable() => Disposition::Failed,
        // Anything else — a 3xx the caller must handle, a 4xx status this table does not name — is a
        // final answer. Refusing to retry an unknown status is the fail-closed direction: trying a
        // request the caller would have to handle anyway is the one mistake that costs money.
        _ => Disposition::Final,
    }
}

#[cfg(test)]
mod tests {
    use nico_core::ErrorCategory;

    use super::*;
    use crate::{route::exclusion::Reason, schema::CanonicalError};

    /// A canonical error with the given status and retryability.
    fn error(status: u16, retryable: bool) -> CanonicalError {
        CanonicalError::try_new(ErrorCategory::Upstream, "upstream_error", "failed", status)
            .expect("the fixture status is in 400..=599")
            .with_retryable(retryable)
    }

    #[test]
    fn a_caller_mistake_is_final() {
        for status in [400, 404, 422] {
            assert_eq!(
                classify(status, &error(status, false)),
                Disposition::Final,
                "{status} is the caller's own fault"
            );
        }
    }

    #[test]
    fn a_rate_limit_excludes_the_member_briefly() {
        let disposition = classify(429, &error(429, true));
        assert_eq!(
            disposition,
            Disposition::Exclude {
                reason: Reason::RateLimited,
                cooldown: Duration::from_secs(10),
            }
        );
    }

    #[test]
    fn a_credential_failure_excludes_the_member_for_longer() {
        for status in [401, 403] {
            let disposition = classify(status, &error(status, false));
            assert_eq!(
                disposition,
                Disposition::Exclude {
                    reason: Reason::Credential,
                    cooldown: Duration::from_secs(300),
                },
                "{status} is about the member's credential"
            );
        }
    }

    #[test]
    fn a_retryable_server_failure_is_the_members_problem() {
        assert_eq!(
            classify(502, &error(502, true)),
            Disposition::Failed,
            "the budget decides whether to retry; the breaker counts it"
        );
        assert_eq!(classify(500, &error(500, true)), Disposition::Failed);
    }

    #[test]
    fn an_unknown_answer_is_final_rather_than_retried() {
        // Fail-closed: a status this table does not name is the caller's answer. Trying it again on
        // another provider is the one mistake that costs money without changing the outcome.
        assert_eq!(classify(418, &error(418, false)), Disposition::Final);
        // `retryable` is the provider table's own verdict and stays authoritative for a status this
        // module does not name: the table already decided whether another attempt could succeed, and
        // second-guessing it here would be a second policy for the same question.
        assert_eq!(classify(409, &error(409, true)), Disposition::Failed);
    }
}
