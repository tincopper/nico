//! The expansion: what a publication's hostname members become, and what refuses one.
//!
//! Every test states the DNS facts itself through the injected resolver, so the scenarios run
//! without a resolver, without a network, and without assuming what `localhost` means. The
//! production wrapper is one line over [`super::expand_members_with`] and is exercised by the
//! pipeline's own tests.

use std::{
    io,
    net::{IpAddr, Ipv4Addr},
    pin::Pin,
    sync::Arc,
};

use nico_core::ErrorCategory;

use super::*;
use crate::test_support::document_with;

/// A document whose upstreams are `upstreams` and whose routes are empty.
fn published(upstreams: &str) -> Document {
    document_with(upstreams, "  []")
}

/// One address in the range the tests use, so failures read as the machine they are.
fn machine(last: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(10, 0, 0, last))
}

/// The answers the test resolver gives: two machines for `web.internal`, a repeated one for
/// `api.internal`, one for `db.internal`, and a refusal for any other name.
fn answer(host: &str, _port: u16) -> io::Result<Vec<IpAddr>> {
    match host {
        "web.internal" => Ok(vec![machine(7), machine(8)]),
        "api.internal" => Ok(vec![machine(7), machine(7), machine(9)]),
        "db.internal" => Ok(vec![machine(9)]),
        _ => Err(io::Error::new(io::ErrorKind::NotFound, "no such name")),
    }
}

/// The test resolver: one boxed closure over [`answer`], the shape the expansion consults.
fn resolver() -> MemberResolver {
    Arc::new(move |host: String, port: u16| {
        Box::pin(async move { answer(&host, port) }) as Pin<Box<_>>
    })
}

/// The same resolver, with `web.internal` answering nothing at all.
fn resolver_without_addresses() -> MemberResolver {
    Arc::new(move |host: String, port: u16| {
        Box::pin(async move {
            match host.as_str() {
                "web.internal" => Ok(Vec::new()),
                _ => answer(&host, port),
            }
        }) as Pin<Box<_>>
    })
}

/// A hostname member with two answers becomes two members, keyed by the endpoint's own name.
#[tokio::test]
async fn a_hostname_member_expands_into_one_member_per_address() {
    let document = published(concat!(
        "  - name: api-backend\n",
        "    scheme: http\n",
        "    host: api.example.test\n",
        "    port: 8080\n",
        "    endpoints:\n",
        "      - name: web\n",
        "        address: web.internal\n",
        "        weight: 3\n",
    ));

    let expanded = expand_members_with(&document, &resolver())
        .await
        .expect("resolves");

    let addresses: Vec<String> = expanded
        .addresses("api-backend", "web")
        .expect("the hostname member is in the expansion")
        .iter()
        .map(|address| address.to_string())
        .collect();
    assert_eq!(
        addresses,
        ["10.0.0.7", "10.0.0.8"],
        "each answer is one member, in the resolver's order"
    );
}

/// A written literal names its machine already: it stays out of the expansion, and its member is
/// the address as written.
#[tokio::test]
async fn a_literal_member_stays_out_of_the_expansion() {
    let document = published(concat!(
        "  - name: api-backend\n",
        "    scheme: http\n",
        "    host: api.example.test\n",
        "    port: 8080\n",
        "    endpoints:\n",
        "      - name: one\n",
        "        address: 10.0.0.1\n",
    ));

    let expanded = expand_members_with(&document, &resolver())
        .await
        .expect("resolves");

    assert!(
        expanded.addresses("api-backend", "one").is_none(),
        "a literal member has no expansion entry: its own address is the member"
    );
}

/// The resolver failing fails the candidate under the endpoint's own field.
#[tokio::test]
async fn a_failed_resolution_names_the_endpoint_field() {
    let document = published(concat!(
        "  - name: api-backend\n",
        "    scheme: http\n",
        "    host: api.example.test\n",
        "    port: 8080\n",
        "    endpoints:\n",
        "      - name: web\n",
        "        address: absent.internal\n",
        "      - name: one\n",
        "        address: 10.0.0.1\n",
    ));

    let error = expand_members_with(&document, &resolver())
        .await
        .expect_err("the resolver refuses `absent.internal`");

    assert_eq!(
        error,
        MemberExpansionError::Unresolved {
            field: "upstreams[0].endpoints[0].address".to_owned(),
            reason: "no such name".to_owned(),
        },
        "the refusal names the field that cannot be resolved"
    );
    assert_eq!(
        error.category(),
        ErrorCategory::Upstream,
        "a resolver that did not answer is the dependency's failure"
    );
}

/// A name that answers with no address is the same refusal: one machine was expected and none is
/// usable.
#[tokio::test]
async fn an_empty_answer_is_a_refusal_too() {
    let document = published(concat!(
        "  - name: api-backend\n",
        "    scheme: http\n",
        "    host: api.example.test\n",
        "    port: 8080\n",
        "    endpoints:\n",
        "      - name: web\n",
        "        address: web.internal\n",
    ));

    let error = expand_members_with(&document, &resolver_without_addresses())
        .await
        .expect_err("an answer with no address yields no member");

    assert!(matches!(error, MemberExpansionError::Unresolved { .. }));
}

/// The same name answering twice with one address is one member, not two.
#[tokio::test]
async fn repeated_answers_for_one_name_yield_one_member() {
    let document = published(concat!(
        "  - name: api-backend\n",
        "    scheme: http\n",
        "    host: api.example.test\n",
        "    port: 8080\n",
        "    endpoints:\n",
        "      - name: api\n",
        "        address: api.internal\n",
    ));

    let expanded = expand_members_with(&document, &resolver())
        .await
        .expect("resolves");

    let addresses: Option<Vec<String>> = expanded
        .addresses("api-backend", "api")
        .map(|list| list.iter().map(|address| address.to_string()).collect());
    assert_eq!(
        addresses,
        Some(vec!["10.0.0.7".to_owned(), "10.0.0.9".to_owned()]),
        "the repeated answer collapses; the distinct machines stay"
    );
}

/// A hostname that resolves onto a machine a written member names is refused: one machine is one
/// member, and a duplicated one would split its failures across two counters.
#[tokio::test]
async fn a_hostname_resolving_onto_a_written_member_is_refused() {
    let document = published(concat!(
        "  - name: api-backend\n",
        "    scheme: http\n",
        "    host: api.example.test\n",
        "    port: 8080\n",
        "    endpoints:\n",
        "      - name: one\n",
        "        address: 10.0.0.7\n",
        "      - name: web\n",
        "        address: web.internal\n",
    ));

    let error = expand_members_with(&document, &resolver())
        .await
        .expect_err("`web.internal` resolves onto the machine `one` names");

    assert!(
        matches!(error, MemberExpansionError::Collision { .. }),
        "expected Collision, got {error:?}"
    );
    assert_eq!(
        error.category(),
        ErrorCategory::Client,
        "two members naming one machine is the document's mistake"
    );
    let message = error.to_string();
    assert!(
        message.contains("upstreams[0].endpoints[1].address") && message.contains("`one`"),
        "the refusal names the colliding field and the member that got there first, got {message:?}"
    );
}

/// Two names resolving to the same machine are refused the same way, whichever endpoint is second.
#[tokio::test]
async fn two_names_on_one_machine_are_refused() {
    let document = published(concat!(
        "  - name: api-backend\n",
        "    scheme: http\n",
        "    host: api.example.test\n",
        "    port: 8080\n",
        "    endpoints:\n",
        "      - name: api\n",
        "        address: api.internal\n",
        "      - name: db\n",
        "        address: db.internal\n",
    ));

    let error = expand_members_with(&document, &resolver())
        .await
        .expect_err("both names answer `10.0.0.9`");

    assert!(
        matches!(error, MemberExpansionError::Collision { .. }),
        "expected Collision, got {error:?}"
    );
}

/// An upstream whose members are all literals expands to nothing and validates cleanly: today's
/// documents publish unchanged.
#[tokio::test]
async fn a_document_without_hostnames_expands_to_an_empty_view() {
    let document = published(concat!(
        "  - name: api-backend\n",
        "    scheme: http\n",
        "    host: api.example.test\n",
        "    port: 8080\n",
        "    endpoints:\n",
        "      - name: one\n",
        "        address: 10.0.0.1\n",
        "      - name: two\n",
        "        address: 10.0.0.2\n",
    ));

    let expanded = expand_members_with(&document, &resolver())
        .await
        .expect("resolves");

    assert_eq!(
        expanded,
        ExpandedMembers::default(),
        "no hostname, no expansion entry: the view is the empty default"
    );
}
