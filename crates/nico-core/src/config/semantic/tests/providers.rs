use super::*;

#[test]
fn a_base_url_without_an_http_scheme_is_rejected() {
    let providers = provider_entry("ftp://api.openai.com", "env:OPENAI_API_KEY");
    let error =
        parse(&document(&providers, NO_ENTRIES, NO_ENTRIES)).expect_err("non-HTTP base URL");
    assert!(
        matches!(
            error,
            DocumentError::InvalidField {
                field: "providers[].base_url",
                ..
            }
        ),
        "expected InvalidField for providers[].base_url, got {error:?}"
    );
}

#[test]
fn a_base_url_with_whitespace_is_rejected() {
    let providers = provider_entry("https://api openai.com", "env:OPENAI_API_KEY");
    let error =
        parse(&document(&providers, NO_ENTRIES, NO_ENTRIES)).expect_err("whitespace in URL");
    assert!(
        matches!(
            error,
            DocumentError::InvalidField {
                field: "providers[].base_url",
                ..
            }
        ),
        "expected InvalidField for providers[].base_url, got {error:?}"
    );
}

#[test]
fn a_credential_ref_without_a_known_prefix_is_rejected() {
    let providers = provider_entry("https://api.openai.com", "OPENAI_API_KEY");
    let error = parse(&document(&providers, NO_ENTRIES, NO_ENTRIES))
        .expect_err("a literal credential is not a reference");
    assert!(
        matches!(
            error,
            DocumentError::InvalidField {
                field: "providers[].credential_ref",
                ..
            }
        ),
        "expected InvalidField for providers[].credential_ref, got {error:?}"
    );
}

/// A provider entry whose `model_map` body is spliced verbatim under the field.
fn provider_with_model_map(body: &str) -> String {
    format!(
        concat!(
            "  - name: openai-primary\n",
            "    kind: openai\n",
            "    base_url: https://api.openai.com\n",
            "    credential_ref: env:OPENAI_API_KEY\n",
            "    model_map:\n",
            "{}",
        ),
        body
    )
}

/// Asserts that `body` is refused, and that the message names the side that broke.
fn assert_map_rejected(body: &str, expected_reason: &str) {
    let providers = provider_with_model_map(body);
    let error = parse(&document(&providers, NO_ENTRIES, NO_ENTRIES))
        .expect_err("a malformed mapping entry");
    match error {
        DocumentError::InvalidField {
            field: "providers[].model_map",
            reason,
        } => assert!(
            reason.contains(expected_reason),
            "expected a reason containing {expected_reason:?}, got {reason:?}"
        ),
        other => panic!("expected InvalidField for providers[].model_map, got {other:?}"),
    }
}

#[test]
fn an_omitted_model_map_maps_nothing() {
    let providers = provider_entry("https://api.openai.com", "env:OPENAI_API_KEY");
    let document = parse(&document(&providers, NO_ENTRIES, NO_ENTRIES)).expect("no map is legal");
    assert!(
        document.providers()[0].model_map().is_empty(),
        "without a map the caller's model name is sent unchanged"
    );
}

#[test]
fn model_names_are_not_held_to_the_kebab_case_rule() {
    // A model name is the vendor's spelling, not a resource name this document invents: a version
    // dot, a path segment and a date suffix all appear in real names.
    let providers = provider_with_model_map("      fast.v2: vendor/model:2024-09\n");
    let document = parse(&document(&providers, NO_ENTRIES, NO_ENTRIES)).expect("legal names");
    assert_eq!(
        document.providers()[0]
            .model_map()
            .get("fast.v2")
            .map(String::as_str),
        Some("vendor/model:2024-09")
    );
}

#[test]
fn an_empty_mapping_key_is_rejected() {
    assert_map_rejected("      \"\": small-model\n", "keys must not be empty");
}

#[test]
fn an_empty_mapping_value_is_rejected() {
    assert_map_rejected("      fast: \"\"\n", "values must not be empty");
}

#[test]
fn a_mapping_key_with_whitespace_is_rejected() {
    assert_map_rejected(
        "      \"fast model\": small-model\n",
        "keys must not contain whitespace",
    );
}

#[test]
fn a_mapping_value_with_a_control_character_is_rejected() {
    // The YAML escape becomes a real tab: a name carrying one would be reproduced in a header or a
    // log line in a form nothing can read back.
    assert_map_rejected(
        "      fast: \"small\\tmodel\"\n",
        "values must not contain whitespace",
    );
}

/// Appends a `circuit_breaker` section to the fixture provider.
fn provider_with_breaker(section: &str) -> String {
    format!(
        "{}{}",
        provider_entry("https://api.openai.com", "env:OPENAI_API_KEY"),
        section
    )
}

#[test]
fn a_breaker_threshold_above_one_thousand_is_rejected() {
    let providers = provider_with_breaker("    circuit_breaker:\n      failure_threshold: 1001\n");
    let error = parse(&document(&providers, NO_ENTRIES, NO_ENTRIES))
        .expect_err("a threshold past the bound is a number, not a configuration");
    assert!(
        matches!(
            error,
            DocumentError::LimitOutOfRange {
                field: "providers[].circuit_breaker.failure_threshold",
                value: 1001
            }
        ),
        "expected LimitOutOfRange for failure_threshold, got {error:?}"
    );
    assert_eq!(error.category(), ErrorCategory::Limit);
}

#[test]
fn a_zero_breaker_threshold_is_accepted() {
    // Zero is the one spelling for "never stop calling this endpoint": the breaker is disabled, and
    // refusing it would leave no way to express that.
    let providers = provider_with_breaker("    circuit_breaker:\n      failure_threshold: 0\n");
    parse(&document(&providers, NO_ENTRIES, NO_ENTRIES))
        .expect("a disabled breaker is a configuration");
}

#[test]
fn a_breaker_threshold_at_the_bound_is_accepted() {
    let providers = provider_with_breaker("    circuit_breaker:\n      failure_threshold: 1000\n");
    parse(&document(&providers, NO_ENTRIES, NO_ENTRIES))
        .expect("the last accepted threshold is a configuration");
}

#[test]
fn a_breaker_cooldown_at_the_bound_is_accepted() {
    let providers = provider_with_breaker("    circuit_breaker:\n      cooldown_ms: 600000\n");
    parse(&document(&providers, NO_ENTRIES, NO_ENTRIES))
        .expect("a ten-minute cooldown is a configuration");
}

#[test]
fn a_zero_breaker_cooldown_is_rejected() {
    let providers = provider_with_breaker("    circuit_breaker:\n      cooldown_ms: 0\n");
    let error = parse(&document(&providers, NO_ENTRIES, NO_ENTRIES))
        .expect_err("an open breaker must stay open long enough to be observable");
    assert!(
        matches!(
            error,
            DocumentError::LimitOutOfRange {
                field: "providers[].circuit_breaker.cooldown_ms",
                value: 0
            }
        ),
        "expected LimitOutOfRange for cooldown_ms, got {error:?}"
    );
}

#[test]
fn a_breaker_cooldown_above_ten_minutes_is_rejected() {
    let providers = provider_with_breaker("    circuit_breaker:\n      cooldown_ms: 600001\n");
    let error = parse(&document(&providers, NO_ENTRIES, NO_ENTRIES))
        .expect_err("a cooldown past the bound is a maintenance window");
    assert!(
        matches!(
            error,
            DocumentError::LimitOutOfRange {
                field: "providers[].circuit_breaker.cooldown_ms",
                value: 600_001
            }
        ),
        "expected LimitOutOfRange for cooldown_ms, got {error:?}"
    );
}
