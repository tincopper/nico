//! The exclusion table: what it retires, for how long, and what it never confuses with a breaker.
//!
//! Every test states a fact about time, so the clock is an argument: an entry that is out at `t` and
//! in at `t + cooldown` is the whole contract, and a test that slept would assert the same thing more
//! slowly.

use tokio::time::Instant;

use super::*;

/// An entry naming one member of one route.
fn entry() -> Entry {
    Entry::new("llm-chat", "primary")
}

#[test]
fn an_empty_table_admits_everything() {
    let table = ExclusionTable::new();
    let now = Instant::now();

    assert!(table.admits(&entry(), now));
    assert!(table.is_empty());
}

#[test]
fn a_retired_member_is_out_until_its_cooldown_passes() {
    let table = ExclusionTable::new();
    let now = Instant::now();

    let until = table.exclude(entry(), Reason::RateLimited, now);

    assert_eq!(until, now + Duration::from_secs(10));
    assert!(!table.admits(&entry(), now));
    assert!(
        !table.admits(&entry(), until - Duration::from_millis(1)),
        "the member is out right up to the instant its cooldown ends"
    );
    assert!(
        table.admits(&entry(), until),
        "the cooldown is inclusive at its end"
    );
}

#[test]
fn a_credential_failure_retires_for_longer_than_a_rate_limit() {
    let table = ExclusionTable::new();
    let now = Instant::now();

    let limited = table.exclude(Entry::new("a", "one"), Reason::RateLimited, now);
    let credential = table.exclude(Entry::new("a", "two"), Reason::Credential, now);

    assert!(
        credential > limited,
        "a broken key outlives a spent rate-limit window: {credential:?} vs {limited:?}"
    );
}

#[test]
fn one_route_retiring_a_member_does_not_retire_it_for_another() {
    // The same provider may be a pool member on one route and not on another. Retiring it everywhere
    // would turn one route's rate limit into a deployment-wide outage.
    let table = ExclusionTable::new();
    let now = Instant::now();

    table.exclude(Entry::new("llm-chat", "shared"), Reason::RateLimited, now);

    assert!(!table.admits(&Entry::new("llm-chat", "shared"), now));
    assert!(
        table.admits(&Entry::new("llm-embed", "shared"), now),
        "another route still distributes to the same provider"
    );
}

#[test]
fn an_expired_entry_is_dropped_on_read() {
    // The table holds only members that are actually out, so it cannot grow with every rate limit the
    // deployment ever saw.
    let table = ExclusionTable::new();
    let now = Instant::now();
    let until = table.exclude(entry(), Reason::RateLimited, now);

    assert!(table.admits(&entry(), until));

    assert!(
        table.is_empty(),
        "reading past the cooldown removes the entry instead of leaving it to accumulate"
    );
}

#[test]
fn a_sweep_drops_expired_entries_and_keeps_live_ones() {
    let table = ExclusionTable::new();
    let now = Instant::now();
    table.exclude(Entry::new("a", "expired"), Reason::RateLimited, now);
    table.exclude(Entry::new("a", "live"), Reason::Credential, now);

    let dropped = table.sweep(now + Duration::from_secs(11));

    assert_eq!(dropped, 1, "only the rate-limited entry expired");
    assert!(
        !table.admits(&Entry::new("a", "live"), now + Duration::from_secs(11)),
        "the credential entry still has most of its five minutes left"
    );
    assert_eq!(
        table.len(),
        1,
        "a sweep that kept the live entry left exactly one row"
    );
}

#[test]
fn a_second_exclusion_restarts_the_cooldown() {
    let table = ExclusionTable::new();
    let now = Instant::now();
    table.exclude(entry(), Reason::RateLimited, now);

    let later = now + Duration::from_secs(8);
    let until = table.exclude(entry(), Reason::RateLimited, later);

    assert_eq!(
        until,
        later + Duration::from_secs(10),
        "the newer observation is the better one, so the window restarts rather than accumulating"
    );
}

#[test]
fn a_retired_entry_reports_why_and_until_when() {
    let table = ExclusionTable::new();
    let now = Instant::now();
    let until = table.exclude(entry(), Reason::Credential, now);

    assert_eq!(table.retired(&entry()), Some((until, Reason::Credential)));
    assert_eq!(
        Reason::Credential.as_str(),
        "credential",
        "the label an operator reads"
    );
    assert_eq!(Reason::RateLimited.as_str(), "rate_limited");
}

#[test]
fn the_entry_reports_the_names_it_was_built_from() {
    let entry = Entry::new("llm-chat", "primary");

    assert_eq!(entry.route(), "llm-chat");
    assert_eq!(entry.provider(), "primary");
}
