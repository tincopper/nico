//! Tests for the pipeline's public failure vocabulary.
//!
//! The pacing arithmetic has its own tests next to it (`pacing::tests`); what is verified here is the
//! classification an operator's exit code and alerting depend on: which failure is the writer's
//! mistake, which is a dependency, and which is this process's own defect.

use nico_config::ConfigSourceError;
use nico_core::{ErrorCategory, Generation};

use super::ReloadError;

#[test]
fn an_unreachable_source_makes_the_pipeline_not_ready_upstream() {
    let error = ReloadError::NotReady {
        category: ErrorCategory::Upstream,
    };
    assert_eq!(error.category(), ErrorCategory::Upstream);
}

#[test]
fn a_source_error_keeps_its_own_category() {
    let error = ReloadError::from(ConfigSourceError::TooLarge { size: 9, limit: 4 });
    assert_eq!(error.category(), ErrorCategory::Limit);
    assert!(
        matches!(error, ReloadError::Source { .. }),
        "expected Source, got {error:?}"
    );
}

#[test]
fn an_exhausted_generation_is_internal() {
    let overflow = Generation::new(u64::MAX)
        .next()
        .expect_err("the counter is exhausted");
    let error = ReloadError::from(overflow);
    assert_eq!(error.category(), ErrorCategory::Internal);
    assert!(
        matches!(error, ReloadError::GenerationOverflow { .. }),
        "expected GenerationOverflow, got {error:?}"
    );
}
