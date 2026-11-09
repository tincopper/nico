//! Range tests for `routes[].fallback`: how many targets one route may name after its own.
//!
//! The chain length is one factor of the data plane's attempt fuse ([`MAX_ATTEMPTS_PER_ROUTE`]), so a
//! document that could outrun the loop is refused by the model rather than discovered as a truncated
//! sequence at request time. These fixtures build their own provider set: the length is what is under
//! test, so the number of declared providers must vary with it.

use super::*;

/// A document whose provider set holds `members` entries, and one route chaining every entry after
/// the first.
fn document_with_chain(members: usize) -> String {
    let providers: String = (1..=members)
        .map(|index| {
            format!(
                concat!(
                    "  - name: target-{index}\n",
                    "    kind: openai\n",
                    "    base_url: http://127.0.0.1:8080\n",
                    "    credential_ref: env:TARGET_{index}_KEY\n",
                ),
                index = index
            )
        })
        .collect();
    let chain: Vec<String> = (2..=members)
        .map(|index| format!("target-{index}"))
        .collect();
    format!(
        concat!(
            "apiVersion: gateway/v1\n",
            "request:\n",
            "  max_body_bytes: 1048576\n",
            "  timeout_ms: 30000\n",
            "plugins: []\n",
            "providers:\n",
            "{providers}",
            "upstreams: []\n",
            "routes:\n",
            "  - id: llm-chat\n",
            "    path_prefix: /v1/chat/\n",
            "    provider: target-1\n",
            "    protocol: openai.chat\n",
            "    retry_budget: {budget}\n",
            "    fallback: [{chain}]\n",
        ),
        providers = providers,
        budget = MAX_RETRY_BUDGET,
        chain = chain.join(", "),
    )
}

#[test]
fn the_longest_route_the_data_plane_is_sized_for_parses() {
    // One target plus every member the model allows, absorbing the largest budget it allows: this is
    // exactly the sequence `MAX_ATTEMPTS_PER_ROUTE` covers, so accepting it is what makes the fuse
    // derived from these bounds sufficient.
    let document = document_with_chain(1 + MAX_FALLBACK_MEMBERS);
    let parsed = parse(&document)
        .unwrap_or_else(|error| panic!("the longest legal route must parse: {error:?}"));
    assert_eq!(parsed.routes()[0].fallback().len(), MAX_FALLBACK_MEMBERS);
}

#[test]
fn a_chain_one_member_past_the_bound_is_rejected() {
    let over = 1 + MAX_FALLBACK_MEMBERS + 1;
    let error = parse(&document_with_chain(over))
        .expect_err("a chain longer than the bound would outrun the attempt fuse");
    assert!(
        matches!(
            error,
            DocumentError::LimitOutOfRange {
                field: "routes[].fallback",
                value
            } if value == (MAX_FALLBACK_MEMBERS + 1) as u64
        ),
        "expected LimitOutOfRange for routes[].fallback, got {error:?}"
    );
    assert_eq!(error.category(), ErrorCategory::Limit);
}
