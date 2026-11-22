//! Selection state: what a publication inherits, and what one generation selects from.
//!
//! The tests fall into three groups, and the split is the design:
//!
//! - **what a request selects** — the destination a member produces, and one sequence per strategy;
//! - **what survives a publication** — the same identity keeps its counters and the rotation position,
//!   a changed address starts from zero, a removed member is forgotten;
//! - **what cannot be selected** — no member at all is `None`, which the request path turns into a
//!   `502` rather than sending the request to a member it just decided against.

use std::{
    collections::BTreeSet,
    net::{IpAddr, Ipv4Addr},
    sync::Arc,
    time::Instant,
};

use nico_core::{Document, Scheme};

mod expansion;
mod health;
mod route_pool;

use super::*;
use crate::{
    test_support::{API_BACKEND, document_with},
    upstream::dns::ExpandedMembers,
};

/// A document whose upstreams are `upstreams` and whose routes are empty.
fn published(upstreams: &str) -> Document {
    document_with(upstreams, "  []")
}

const THREE_MEMBERS: &str = concat!(
    "  - name: api-backend\n",
    "    scheme: http\n",
    "    host: api.example.test\n",
    "    port: 8080\n",
    "    endpoints:\n",
    "      - name: one\n",
    "        address: 10.0.0.1\n",
    "      - name: two\n",
    "        address: 10.0.0.2\n",
    "      - name: three\n",
    "        address: 10.0.0.3\n",
);

/// [`THREE_MEMBERS`] with weights, for the smooth rotation.
const WEIGHTED_MEMBERS: &str = concat!(
    "  - name: api-backend\n",
    "    scheme: http\n",
    "    host: api.example.test\n",
    "    port: 8080\n",
    "    strategy: weighted_round_robin\n",
    "    endpoints:\n",
    "      - name: one\n",
    "        address: 10.0.0.1\n",
    "        weight: 3\n",
    "      - name: two\n",
    "        address: 10.0.0.2\n",
    "      - name: three\n",
    "        address: 10.0.0.3\n",
);

/// Two members, for the strategies whose answer depends on what the others are doing.
const TWO_MEMBERS: &str = concat!(
    "  - name: api-backend\n",
    "    scheme: http\n",
    "    host: api.example.test\n",
    "    port: 8080\n",
    "    strategy: least_conn\n",
    "    endpoints:\n",
    "      - name: one\n",
    "        address: 10.0.0.1\n",
    "      - name: two\n",
    "        address: 10.0.0.2\n",
);

/// Two members chosen by caller address.
const HASHED_MEMBERS: &str = concat!(
    "  - name: api-backend\n",
    "    scheme: http\n",
    "    host: api.example.test\n",
    "    port: 8080\n",
    "    strategy: ip_hash\n",
    "    endpoints:\n",
    "      - name: one\n",
    "        address: 10.0.0.1\n",
    "      - name: two\n",
    "        address: 10.0.0.2\n",
    "      - name: three\n",
    "        address: 10.0.0.3\n",
);

/// [`THREE_MEMBERS`] with the first member moved to another machine.
const MOVED_FIRST_MEMBER: &str = concat!(
    "  - name: api-backend\n",
    "    scheme: http\n",
    "    host: api.example.test\n",
    "    port: 8080\n",
    "    endpoints:\n",
    "      - name: one\n",
    "        address: 10.0.0.9\n",
    "      - name: two\n",
    "        address: 10.0.0.2\n",
    "      - name: three\n",
    "        address: 10.0.0.3\n",
);

/// Selects one member of `state`, panicking when it has none.
///
/// `now` is the instant the availability verdict is asked at. The fixtures here never eject a member,
/// so the wall clock is the honest answer; the ejection scenarios live in [`health`](health) and pass
/// the instants they mean.
fn select(state: &UpstreamState) -> Selected {
    select_at(state, Instant::now())
}

/// Selects one member at `now`.
fn select_at(state: &UpstreamState, now: Instant) -> Selected {
    state
        .select(None, now)
        .expect("every member of the fixture is available")
}

/// Selects one member for a caller address.
fn select_for(state: &UpstreamState, caller: IpAddr) -> Selected {
    state
        .select(Some(caller), Instant::now())
        .expect("every member of the fixture is available")
}

/// Returns the state `table` published for the fixture upstream.
fn upstream(table: &SelectionTable) -> &Arc<UpstreamState> {
    table
        .upstream("api-backend")
        .expect("the fixture publishes api-backend")
}

#[test]
fn a_single_address_upstream_selects_its_only_member() {
    let pools = UpstreamPools::default();
    let table = pools.publish(&published(API_BACKEND), &ExpandedMembers::default());

    let selected = select(upstream(&table));

    assert_eq!(selected.destination.scheme(), Scheme::Http);
    assert_eq!(selected.destination.host(), "127.0.0.1");
    assert_eq!(selected.destination.port(), 8080);
    assert_eq!(
        selected.destination.authority(),
        "127.0.0.1",
        "one address means one name, exactly as before members existed"
    );
}

#[test]
fn a_member_is_addressed_by_the_upstreams_logical_host() {
    let pools = UpstreamPools::default();
    let table = pools.publish(&published(THREE_MEMBERS), &ExpandedMembers::default());

    let selected = select(upstream(&table));

    assert_eq!(
        selected.destination.host(),
        "10.0.0.1",
        "the connection target is the member's address"
    );
    assert_eq!(
        selected.destination.authority(),
        "api.example.test",
        "Host and SNI stay the upstream's host, never the machine"
    );
}

#[test]
fn round_robin_walks_the_members_in_document_order() {
    let pools = UpstreamPools::default();
    let table = pools.publish(&published(THREE_MEMBERS), &ExpandedMembers::default());
    let state = upstream(&table);

    let hosts: Vec<String> = (0..4)
        .map(|_| select(state).destination.host().to_owned())
        .collect();

    assert_eq!(hosts, ["10.0.0.1", "10.0.0.2", "10.0.0.3", "10.0.0.1"]);
}

#[test]
fn weighted_round_robin_follows_the_document_weights() {
    let pools = UpstreamPools::default();
    let table = pools.publish(&published(WEIGHTED_MEMBERS), &ExpandedMembers::default());
    let state = upstream(&table);

    let hosts: Vec<String> = (0..5)
        .map(|_| select(state).destination.host().to_owned())
        .collect();

    assert_eq!(
        hosts,
        ["10.0.0.1", "10.0.0.2", "10.0.0.1", "10.0.0.3", "10.0.0.1"],
        "3:1:1 interleaves instead of sending three requests to one member first"
    );
}

#[test]
fn least_conn_moves_away_from_a_member_that_is_still_serving() {
    let pools = UpstreamPools::default();
    let table = pools.publish(&published(TWO_MEMBERS), &ExpandedMembers::default());
    let state = upstream(&table);

    let first = select(state);
    let second = select(state);

    assert_eq!(first.destination.host(), "10.0.0.1");
    assert_eq!(
        second.destination.host(),
        "10.0.0.2",
        "the first member was busy, so the tie no longer applies"
    );
    assert_eq!(state.members()[0].state().active_connections(), 1);
}

#[test]
fn dropping_a_selection_releases_its_in_flight_count() {
    let pools = UpstreamPools::default();
    let table = pools.publish(&published(TWO_MEMBERS), &ExpandedMembers::default());
    let state = upstream(&table);

    let selected = select(state);
    assert_eq!(state.members()[0].state().active_connections(), 1);

    drop(selected);
    assert_eq!(
        state.members()[0].state().active_connections(),
        0,
        "Drop is the only decrement, so every exit path releases the count"
    );
}

#[test]
fn ip_hash_keeps_one_caller_on_one_member() {
    let pools = UpstreamPools::default();
    let table = pools.publish(&published(HASHED_MEMBERS), &ExpandedMembers::default());
    let state = upstream(&table);
    let caller = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7));

    let first = select_for(state, caller).destination.host().to_owned();
    let repeated: Vec<String> = (0..4)
        .map(|_| select_for(state, caller).destination.host().to_owned())
        .collect();

    assert!(
        repeated.iter().all(|host| host == &first),
        "one caller must stay on one member, went to {repeated:?} after {first}"
    );
}

#[test]
fn ip_hash_spreads_different_callers_across_the_members() {
    let pools = UpstreamPools::default();
    let table = pools.publish(&published(HASHED_MEMBERS), &ExpandedMembers::default());
    let state = upstream(&table);

    let reached: BTreeSet<String> = (1..=32)
        .map(|last| {
            let caller = IpAddr::V4(Ipv4Addr::new(198, 51, 100, last));
            select_for(state, caller).destination.host().to_owned()
        })
        .collect();

    assert_eq!(
        reached.len(),
        3,
        "32 callers should reach every member, reached {reached:?}"
    );
}

#[test]
fn a_member_that_kept_its_identity_keeps_its_state() {
    let pools = UpstreamPools::default();
    let first_table = pools.publish(&published(THREE_MEMBERS), &ExpandedMembers::default());
    let held = select(upstream(&first_table));

    let second_table = pools.publish(&published(THREE_MEMBERS), &ExpandedMembers::default());
    let inherited = upstream(&second_table);

    assert!(
        Arc::ptr_eq(
            upstream(&first_table).members()[0].state(),
            inherited.members()[0].state()
        ),
        "the same identity must keep the same state, not a copy of its counters"
    );
    assert_eq!(
        inherited.members()[0].state().active_connections(),
        1,
        "the request the first generation is still serving stays counted"
    );
    drop(held);
}

#[test]
fn a_member_whose_address_changed_starts_from_zero() {
    let pools = UpstreamPools::default();
    let first_table = pools.publish(&published(THREE_MEMBERS), &ExpandedMembers::default());
    let held = select(upstream(&first_table));

    let second_table = pools.publish(&published(MOVED_FIRST_MEMBER), &ExpandedMembers::default());
    let moved = upstream(&second_table);

    assert!(
        !Arc::ptr_eq(
            upstream(&first_table).members()[0].state(),
            moved.members()[0].state()
        ),
        "another machine is another member, however it is named"
    );
    assert_eq!(
        moved.members()[0].state().active_connections(),
        0,
        "a machine that was just written in has served nothing"
    );
    assert_eq!(
        upstream(&first_table).members()[0]
            .state()
            .active_connections(),
        1,
        "the old generation keeps counting against the member it connected to"
    );
    drop(held);
}

#[test]
fn a_removed_member_is_forgotten() {
    let pools = UpstreamPools::default();
    pools.publish(&published(THREE_MEMBERS), &ExpandedMembers::default());
    pools.publish(&published(API_BACKEND), &ExpandedMembers::default());

    assert_eq!(
        pools
            .inherited_ids("api-backend")
            .map(|identities| identities.len()),
        Some(1),
        "the store keeps only what the last publication named"
    );
}

#[test]
fn an_added_member_starts_at_zero_while_the_others_are_inherited() {
    let pools = UpstreamPools::default();
    let first_table = pools.publish(&published(TWO_MEMBERS), &ExpandedMembers::default());
    let held = select(upstream(&first_table));

    let second_table = pools.publish(&published(THREE_MEMBERS), &ExpandedMembers::default());
    let grown = upstream(&second_table);

    assert!(
        Arc::ptr_eq(
            upstream(&first_table).members()[0].state(),
            grown.members()[0].state()
        ),
        "a member that was already there is inherited"
    );
    assert_eq!(
        grown.members()[2].state().active_connections(),
        0,
        "the member just added has served nothing"
    );
    drop(held);
}

#[test]
fn the_rotation_position_survives_a_publication() {
    let pools = UpstreamPools::default();
    let first_table = pools.publish(&published(THREE_MEMBERS), &ExpandedMembers::default());
    let first = select(upstream(&first_table));
    let second = select(upstream(&first_table));

    let second_table = pools.publish(&published(THREE_MEMBERS), &ExpandedMembers::default());
    let next = select(upstream(&second_table));

    assert_eq!(first.destination.host(), "10.0.0.1");
    assert_eq!(second.destination.host(), "10.0.0.2");
    assert_eq!(
        next.destination.host(),
        "10.0.0.3",
        "a reload must not restart the rotation at the first member"
    );
}

#[test]
fn a_generation_with_no_upstream_selects_nothing() {
    let pools = UpstreamPools::default();
    let table = pools.publish(&published("  []"), &ExpandedMembers::default());

    assert_eq!(table.len(), 0);
    assert!(table.upstream("api-backend").is_none());
}

#[test]
fn changing_the_strategy_starts_the_new_one_from_its_own_initial_state() {
    // The old generation was two steps into a rotation. The document then selects by something else:
    // a rotation position says nothing to the new rule, so the new picker must not be handed one.
    // Written as the first choice of each strategy, which differ for these three members: a rotation
    // opens at the first, the smooth rotation opens at the heaviest.
    let pools = UpstreamPools::default();
    let first_table = pools.publish(&published(THREE_MEMBERS), &ExpandedMembers::default());
    let _ = select(upstream(&first_table));
    let _ = select(upstream(&first_table));

    let second_table = pools.publish(&published(WEIGHTED_MEMBERS), &ExpandedMembers::default());
    let next = select(upstream(&second_table));

    assert_eq!(
        next.destination.host(),
        "10.0.0.1",
        "the smooth rotation must start from its own initial state, not a carried position"
    );
}
