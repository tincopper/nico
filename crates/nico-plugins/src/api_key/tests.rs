//! Tests for the API-key filter: where a key is read from, which key set accepts it, and what a
//! refusal is allowed to say.
//!
//! The constant-time comparison is asserted by construction (`constant_time_eq` is the only path a
//! key takes) and by the length cases below: a shorter, a longer and an equal-length-but-wrong key all
//! end in the same decision, so no branch on length can be observed as a different outcome.

use std::sync::Arc;

use nico_http::{HttpError, Outcome, Plugin, PluginContext};
use subtle::ConstantTimeEq;

use crate::{
    api_key::{ApiKeyConfig, ApiKeyPlugin, constant_time_eq},
    spec::ApiKeySpec,
    test_support::{ROUTE, TestContext, document, plugin},
};

/// The header the fixtures' keys arrive in unless a test overrides it.
const HEADER: &str = "authorization";

/// Compiles a document holding `plugins`'s only `api_key` entry.
fn spec(plugins: &str) -> ApiKeySpec {
    let document = document(plugins, &["partner"]);
    let plugin = plugin(&document, "partner");
    let config = ApiKeyConfig::from_spec(plugin).expect("the fixture configuration compiles");
    ApiKeySpec::compile(plugin, &config).expect("the fixture compiles")
}

/// Wraps `secret` as a materialized credential.
fn secret(secret: &str) -> Arc<[u8]> {
    Arc::from(secret.as_bytes())
}

/// Builds one plugin over `spec`, with `secrets` materialized in entry order.
fn filter(spec: &ApiKeySpec, secrets: &[&str]) -> ApiKeyPlugin {
    ApiKeyPlugin::new(spec, secrets.iter().copied().map(secret).collect())
        .expect("the fixture materialized one secret per entry")
}

/// Builds a context for the fixture document's route.
fn fresh() -> TestContext {
    TestContext::new("partner").for_route(ROUTE)
}

/// An `api_key` entry with one key named `acme`, plus whatever extra configuration `extra` holds.
fn entry(extra: &str) -> String {
    format!(
        concat!(
            "  - name: partner\n",
            "    kind: api_key\n",
            "    config:\n",
            "{}",
            "      keys:\n",
            "        - name: acme\n",
            "          secret_ref: env:ACME_KEY\n",
        ),
        extra
    )
}

/// An API-key entry with a tenant grouping dimension.
fn tenant_entry(tenant: &str) -> String {
    format!(
        concat!(
            "  - name: partner\n",
            "    kind: api_key\n",
            "    config:\n",
            "      keys:\n",
            "        - name: acme\n",
            "          tenant: {tenant}\n",
            "          secret_ref: env:ACME_KEY\n",
        ),
        tenant = tenant,
    )
}

/// Runs the request stage, mapping a refusal the way the data plane does.
async fn call(plugin: &ApiKeyPlugin, ctx: &mut TestContext) -> Result<(), HttpError> {
    match plugin.on_request(ctx).await {
        Ok(Outcome::Continue) => Ok(()),
        Ok(other) => panic!("the api_key kind never answers instead of forwarding: {other:?}"),
        Err(error) => Err(HttpError::from(error)),
    }
}

/// Returns the code and category of a refusal.
fn refusal(error: &HttpError) -> (&'static str, nico_core::ErrorCategory) {
    match error {
        HttpError::Rejected { rejection, .. } => (rejection.code(), error.category()),
        other => panic!("expected a compiled rejection, got {other:?}"),
    }
}

mod acceptance {
    use super::*;

    #[tokio::test]
    async fn a_presented_key_establishes_the_entrys_name_as_the_subject() {
        let spec = spec(&entry(""));
        let filter = filter(&spec, &["s3cret"]);
        let mut ctx = fresh().with_header(HEADER, "Bearer s3cret");

        assert!(call(&filter, &mut ctx).await.is_ok());
        assert_eq!(
            ctx.identity(),
            Some("acme"),
            "the key's own name is the subject"
        );
        assert_eq!(
            ctx.plugin_name(),
            "partner",
            "the instance is the source of the identity, so two key sets stay tellable apart"
        );
    }

    #[tokio::test]
    async fn a_key_sets_the_configured_tenant_without_replacing_its_subject() {
        let document = document(&tenant_entry("tenant-a"), &["partner"]);
        let plugin = plugin(&document, "partner");
        let config = ApiKeyConfig::from_spec(plugin).expect("tenant config compiles");
        let spec = ApiKeySpec::compile(plugin, &config).expect("tenant spec compiles");
        let filter = filter(&spec, &["s3cret"]);
        let mut ctx = fresh().with_header(HEADER, "Bearer s3cret");

        assert!(call(&filter, &mut ctx).await.is_ok());
        assert_eq!(ctx.identity(), Some("acme"));
        assert_eq!(ctx.tenant(), Some("tenant-a"));
    }

    #[tokio::test]
    async fn every_configured_key_is_accepted() {
        let document = document(
            concat!(
                "  - name: partner\n",
                "    kind: api_key\n",
                "    config:\n",
                "      keys:\n",
                "        - name: acme\n",
                "          secret_ref: env:ACME_KEY\n",
                "        - name: globex\n",
                "          secret_ref: env:GLOBEX_KEY\n",
            ),
            &["partner"],
        );
        let plugin = plugin(&document, "partner");
        let config = ApiKeyConfig::from_spec(plugin).expect("the fixture configuration compiles");
        let spec = ApiKeySpec::compile(plugin, &config).expect("the fixture compiles");
        let filter = filter(&spec, &["first", "second"]);

        for (key, subject) in [("first", "acme"), ("second", "globex")] {
            let mut ctx = fresh().with_header(HEADER, &format!("Bearer {key}"));
            assert!(
                call(&filter, &mut ctx).await.is_ok(),
                "`{key}` must be accepted"
            );
            assert_eq!(
                ctx.identity(),
                Some(subject),
                "the matching key's own name becomes the subject"
            );
        }
    }

    #[tokio::test]
    async fn a_rotated_key_set_accepts_both_keys() {
        // The documented way to rotate: publish both keys, move the callers over, publish one.
        let document = document(
            concat!(
                "  - name: partner\n",
                "    kind: api_key\n",
                "    config:\n",
                "      keys:\n",
                "        - name: acme-old\n",
                "          secret_ref: env:ACME_OLD\n",
                "        - name: acme-new\n",
                "          secret_ref: env:ACME_NEW\n",
            ),
            &["partner"],
        );
        let plugin = plugin(&document, "partner");
        let config = ApiKeyConfig::from_spec(plugin).expect("the fixture configuration compiles");
        let spec = ApiKeySpec::compile(plugin, &config).expect("the fixture compiles");
        let filter = filter(&spec, &["old-value", "new-value"]);

        for key in ["old-value", "new-value"] {
            let mut ctx = fresh().with_header(HEADER, &format!("Bearer {key}"));
            assert!(
                call(&filter, &mut ctx).await.is_ok(),
                "`{key}` must be accepted"
            );
        }
    }

    #[tokio::test]
    async fn a_custom_header_carries_the_key_verbatim() {
        let spec = spec(&entry("      header: x-api-key\n"));
        let filter = filter(&spec, &["s3cret"]);

        let mut custom = fresh().with_header("x-api-key", "s3cret");
        assert!(
            call(&filter, &mut custom).await.is_ok(),
            "a configured header is the whole value, with no scheme prefix"
        );

        let mut bearer = fresh().with_header(HEADER, "Bearer s3cret");
        assert!(
            call(&filter, &mut bearer).await.is_err(),
            "configured header means the default one is not read"
        );
    }

    #[tokio::test]
    async fn the_bearer_scheme_is_case_insensitive() {
        let spec = spec(&entry(""));
        let filter = filter(&spec, &["s3cret"]);
        let mut ctx = fresh().with_header(HEADER, "bEaReR s3cret");

        assert!(
            call(&filter, &mut ctx).await.is_ok(),
            "RFC 6750 makes the scheme case-insensitive"
        );
    }
}

mod refusals {
    use super::*;

    #[tokio::test]
    async fn a_missing_credential_is_auth_missing_with_a_challenge() {
        let spec = spec(&entry(""));
        let filter = filter(&spec, &["s3cret"]);
        let mut ctx = fresh();

        let error = call(&filter, &mut ctx).await.expect_err("no header");
        let (code, category) = refusal(&error);
        assert_eq!(code, "auth_missing");
        assert_eq!(category, nico_core::ErrorCategory::Client);
        assert_eq!(
            error
                .respond()
                .expect("a rejection is renderable")
                .challenge(),
            Some("Bearer")
        );
        assert!(ctx.identity().is_none());
    }

    #[tokio::test]
    async fn another_scheme_counts_as_no_credential_at_all() {
        let spec = spec(&entry(""));
        let filter = filter(&spec, &["s3cret"]);
        let mut ctx = fresh().with_header(HEADER, "Basic s3cret");

        let error = call(&filter, &mut ctx)
            .await
            .expect_err("not a bearer credential");
        assert_eq!(refusal(&error).0, "auth_missing");
    }

    #[tokio::test]
    async fn a_wrong_key_is_auth_invalid_and_the_key_is_never_echoed() {
        let spec = spec(&entry(""));
        let filter = filter(&spec, &["s3cret"]);
        let mut ctx = fresh().with_header(HEADER, "Bearer not-the-key");

        let error = call(&filter, &mut ctx).await.expect_err("the key is wrong");
        let (code, category) = refusal(&error);
        assert_eq!(code, "auth_invalid");
        assert_eq!(category, nico_core::ErrorCategory::Client);
        let rendered = error.to_string();
        assert!(
            !rendered.contains("not-the-key"),
            "the presented key must not reach the log: {rendered}"
        );
        assert!(
            !rendered.contains("s3cret"),
            "the configured key must not reach the log either: {rendered}"
        );
        assert!(
            ctx.identity().is_none(),
            "a refused request is not authenticated as anybody"
        );
    }

    #[tokio::test]
    async fn a_key_of_a_different_length_is_refused_the_same_way() {
        let spec = spec(&entry(""));
        let filter = filter(&spec, &["s3cret-value"]);

        for presented in [
            "s3cret-valu",   // one byte short
            "s3cret-valuee", // one byte long
            "s3cret-valuf",  // same length, last byte wrong
        ] {
            let mut ctx = fresh().with_header(HEADER, &format!("Bearer {presented}"));
            let error = call(&filter, &mut ctx)
                .await
                .expect_err("a key that is not the configured one must be refused");
            assert_eq!(
                refusal(&error).0,
                "auth_invalid",
                "`{presented}` must be refused as unusable, not as absent"
            );
            assert!(ctx.identity().is_none());
        }
    }

    #[tokio::test]
    async fn a_presentation_longer_than_any_configured_key_is_refused() {
        let spec = spec(&entry(""));
        let filter = filter(&spec, &["s3cret"]);
        let huge = "x".repeat(8 * 1024);
        let mut ctx = fresh().with_header(HEADER, &format!("Bearer {huge}"));

        let error = call(&filter, &mut ctx)
            .await
            .expect_err("no configured key can be this long");
        assert_eq!(refusal(&error).0, "auth_invalid");
    }

    #[tokio::test]
    async fn the_first_matching_key_wins_when_two_entries_share_a_value() {
        // Two names for one value is what a staged rename looks like. The answer must be one of them
        // and must not depend on iteration order beyond "first in the document".
        let document = document(
            concat!(
                "  - name: partner\n",
                "    kind: api_key\n",
                "    config:\n",
                "      keys:\n",
                "        - name: acme\n",
                "          secret_ref: env:ACME_KEY\n",
                "        - name: acme-renamed\n",
                "          secret_ref: env:ACME_KEY\n",
            ),
            &["partner"],
        );
        let plugin = plugin(&document, "partner");
        let config = ApiKeyConfig::from_spec(plugin).expect("the fixture configuration compiles");
        let spec = ApiKeySpec::compile(plugin, &config).expect("the fixture compiles");
        let filter = filter(&spec, &["same", "same"]);

        let mut ctx = fresh().with_header(HEADER, "Bearer same");
        assert!(call(&filter, &mut ctx).await.is_ok());
        assert_eq!(ctx.identity(), Some("acme"));
    }
}

mod comparison {
    use super::*;

    #[test]
    fn equal_values_compare_equal_and_any_difference_does_not() {
        assert!(bool::from(constant_time_eq(b"s3cret", b"s3cret")));
        assert!(!bool::from(constant_time_eq(b"s3cret", b"s3cres")));
        assert!(!bool::from(constant_time_eq(b"s3cret", b"s3cres ")));
        assert!(!bool::from(constant_time_eq(b"s3cret", b"s3cre")));
        assert!(bool::from(constant_time_eq(b"", b"")));
    }

    #[test]
    fn a_length_difference_is_part_of_the_same_decision() {
        // The comparison reports "not equal" for a length difference through the same `Choice` it
        // reports content differences with, so a caller cannot separate the two outcomes.
        let expected = b"0123456789";

        assert!(!bool::from(constant_time_eq(expected, b"012345678")));
        assert!(!bool::from(constant_time_eq(expected, b"01234567890")));
        assert!(bool::from(constant_time_eq(expected, expected)));
    }

    #[test]
    fn the_comparison_is_the_one_subtle_offers() {
        // A doc-level assertion that this crate did not grow a second byte comparison: equal inputs
        // agree with `subtle`, which is the implementation the review approved.
        for (left, right) in [
            (&b"abc"[..], &b"abc"[..]),
            (&b"abc"[..], &b"abd"[..]),
            (&b""[..], &b"a"[..]),
        ] {
            assert_eq!(
                bool::from(constant_time_eq(left, right)),
                bool::from(left.ct_eq(right)),
                "the wrapper must not change the decision"
            );
        }
    }
}
