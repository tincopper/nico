//! The expansion at selection time: a hostname member is one member per resolved address.
//!
//! The resolver is injected, so the member list is a stated fact; what these tests verify is the
//! pool's side of the contract — the members the expansion produced select, inherit and retire by
//! address, exactly like members the operator wrote. The expansion's own refusals are tested in
//! [`dns`](crate::upstream::dns) itself.

use std::{
    collections::BTreeSet,
    net::{IpAddr, Ipv4Addr},
    sync::Arc,
};

use nico_core::Document;

use super::*;
use crate::upstream::dns::{MemberResolver, expand_members_with};

/// One endpoint written as a hostname, so a test can state what the resolver answers for it.
const HOSTNAME_MEMBER: &str = concat!(
    "  - name: api-backend\n",
    "    scheme: http\n",
    "    host: api.example.test\n",
    "    port: 8080\n",
    "    endpoints:\n",
    "      - name: web\n",
    "        address: web.internal\n",
    "        weight: 3\n",
);

/// Expands `document` with a resolver that answers `octets` for any name it is asked about.
///
/// The addresses are `10.0.0.<octet>`, so a failure reads as the machine it is about.
async fn expansion(document: &Document, octets: &[u8]) -> ExpandedMembers {
    let octets = octets.to_vec();
    let resolver: MemberResolver = Arc::new(move |_host: String, _port: u16| {
        let octets = octets.clone();
        Box::pin(async move {
            Ok(octets
                .iter()
                .map(|last| IpAddr::V4(Ipv4Addr::new(10, 0, 0, *last)))
                .collect())
        })
    });
    expand_members_with(document, &resolver)
        .await
        .expect("the injected resolver answers")
}

#[tokio::test]
async fn a_hostname_member_selects_from_each_address_it_resolved_to() {
    let document = published(HOSTNAME_MEMBER);
    let pools = UpstreamPools::default();
    let table = pools.publish(&document, &expansion(&document, &[7, 8]).await);

    let members = upstream(&table).members();
    assert_eq!(
        members.len(),
        2,
        "one member per resolved address, not one member per endpoint"
    );
    let names: BTreeSet<String> = members
        .iter()
        .map(|member| member.state().id().name().to_owned())
        .collect();
    assert_eq!(
        names,
        BTreeSet::from(["web".to_owned()]),
        "the endpoint's name is the role every member plays"
    );
    let addresses: Vec<String> = members
        .iter()
        .map(|member| member.state().id().address().to_string())
        .collect();
    assert_eq!(
        addresses,
        ["10.0.0.7", "10.0.0.8"],
        "the address is the machine; the resolver's order is the rotation order"
    );
    assert!(
        members.iter().all(|member| member.weight() == 3),
        "each member carries the endpoint's weight: the share scales with the address count"
    );
}

#[tokio::test]
async fn an_expanded_member_inherits_by_address_across_publications() {
    let document = published(HOSTNAME_MEMBER);
    let pools = UpstreamPools::default();
    let first = pools.publish(&document, &expansion(&document, &[7, 8]).await);
    let held = select(upstream(&first));

    // The same answers publish the same identities: the machine keeps what the gateway observed
    // about it, however the resolver ordered the answers.
    let second = pools.publish(&document, &expansion(&document, &[7, 8]).await);
    assert!(
        Arc::ptr_eq(
            upstream(&first).members()[0].state(),
            upstream(&second).members()[0].state()
        ),
        "the same address is the same member, not a copy of its counters"
    );

    // An address the name stopped resolving leaves the rotation with its member: the store never
    // guesses which machine replaced it.
    let third = pools.publish(&document, &expansion(&document, &[9]).await);
    assert_eq!(upstream(&third).members().len(), 1);
    let forgotten: Vec<String> = pools
        .inherited_ids("api-backend")
        .expect("the store remembers the upstream")
        .iter()
        .map(|id| id.address().to_string())
        .collect();
    assert_eq!(
        forgotten,
        ["10.0.0.9"],
        "the vanished address is forgotten; inheritance stays exact"
    );

    // An address the name started resolving again is a fresh member: no history to inherit.
    let fourth = pools.publish(&document, &expansion(&document, &[7, 8, 9]).await);
    assert_eq!(upstream(&fourth).members().len(), 3);
    let fresh = upstream(&fourth)
        .members()
        .iter()
        .find(|member| member.state().id().address().to_string() == "10.0.0.8")
        .expect("the returned address is a member again");
    assert_eq!(
        fresh.state().active_connections(),
        0,
        "a machine that was just resolved again has served nothing this generation"
    );
    drop(held);
}
