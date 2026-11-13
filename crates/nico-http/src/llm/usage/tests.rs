//! The token vocabulary: five names, and the difference between "not reported" and "zero".

use super::*;

#[test]
fn every_kind_has_a_distinct_name() {
    let mut names: Vec<&str> = TokenKind::ALL.iter().map(|kind| kind.name()).collect();
    names.sort_unstable();
    names.dedup();

    assert_eq!(
        names.len(),
        TokenKind::ALL.len(),
        "one spelling per kind: the metric label and the log suffix are the same string"
    );
}

#[test]
fn a_name_round_trips_to_its_kind() {
    for kind in TokenKind::ALL {
        assert_eq!(TokenKind::from_name(kind.name()), Some(kind));
    }
    assert_eq!(TokenKind::from_name("total"), None);
}

#[test]
fn an_empty_set_reports_nothing() {
    let usage = TokenUsage::new();

    assert!(usage.is_empty());
    assert_eq!(usage.reported(), []);
}

#[test]
fn an_unreported_kind_is_not_a_zero() {
    // The distinction the whole shape exists for: a consumer that averaged "no answer" as 0 would be
    // computing something nobody asked for.
    let usage = TokenUsage::new().with(TokenKind::Input, 0);

    assert!(!usage.is_empty(), "a reported zero is still a report");
    assert_eq!(usage.get(TokenKind::Input), Some(0));
    assert_eq!(
        usage.get(TokenKind::Output),
        None,
        "an unreported kind has no count at all"
    );
}

#[test]
fn reported_returns_only_the_set_kinds_in_order() {
    let usage = TokenUsage::new()
        .with(TokenKind::Output, 7)
        .with(TokenKind::Input, 11);

    assert_eq!(
        usage.reported(),
        [(TokenKind::Input, 11), (TokenKind::Output, 7)],
        "declaration order, so a log line is stable across requests"
    );
}
