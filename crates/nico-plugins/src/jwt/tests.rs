//! Tests for the HS256 filter: the token shapes it accepts, the ones it refuses, and the fact that a
//! refusal says nothing about which part of the token was wrong.
//!
//! The claim rules are tested against a fixed `now` (the filter's `authenticate` takes one), so
//! `exp`, `nbf` and the leeway are decided by the test rather than by the wall clock. The end-to-end
//! cases go through `on_request`, which uses the real clock, with token lifetimes far enough away that
//! no assertion depends on how long the test takes.

use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use nico_http::{HttpError, Outcome, Plugin, PluginContext};
use sha2::Sha256;

use crate::{
    jwt::{JwtConfig, JwtPlugin},
    spec::JwtSpec,
    test_support::{ROUTE, TestContext, document, plugin},
};

/// The key every fixture signs with unless it says otherwise.
const SECRET: &[u8] = b"a-shared-secret-used-by-the-fixture";

/// The instant the claim tests run at.
const NOW: u64 = 1_700_000_000;

/// A `jwt` entry with one key, plus whatever extra configuration `extra` holds.
fn entry_with(extra: &str) -> String {
    format!(
        concat!(
            "  - name: jwt\n",
            "    kind: jwt\n",
            "    config:\n",
            "      keys:\n",
            "        - kid: k1\n",
            "          secret_ref: env:JWT_SECRET\n",
            "{}",
        ),
        extra
    )
}

/// A `jwt` entry with two keys, plus whatever extra configuration `extra` holds.
fn entry_with_keys(extra: &str) -> String {
    format!(
        concat!(
            "  - name: jwt\n",
            "    kind: jwt\n",
            "    config:\n",
            "      keys:\n",
            "        - kid: k1\n",
            "          secret_ref: env:FIRST\n",
            "        - kid: k2\n",
            "          secret_ref: env:SECOND\n",
            "{}",
        ),
        extra
    )
}

/// Compiles `entry` and builds the plugin over `secrets`.
fn filter(entry: &str, secrets: &[&[u8]]) -> JwtPlugin {
    let document = document(entry, &["jwt"]);
    let plugin = plugin(&document, "jwt");
    let config = JwtConfig::from_spec(plugin).expect("the fixture configuration compiles");
    let spec = JwtSpec::compile(plugin, &config).expect("the fixture compiles");
    JwtPlugin::new(
        &spec,
        secrets.iter().map(|secret| Arc::from(*secret)).collect(),
    )
    .expect("the fixture materialized one secret per key")
}

/// The default plugin: one key, no pinned claims.
fn one_key() -> JwtPlugin {
    filter(&entry_with(""), &[SECRET])
}

/// Returns the fixture instant.
fn now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(NOW)
}

/// Signs `header` and `payload` with `secret`, producing a token.
fn sign(header: &str, payload: &str, secret: &[u8]) -> String {
    let header = URL_SAFE_NO_PAD.encode(header);
    let payload = URL_SAFE_NO_PAD.encode(payload);
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(format!("{header}.{payload}").as_bytes());
    let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    format!("{header}.{payload}.{signature}")
}

/// Signs a header naming `kid` and claims holding `exp` plus `extra`.
fn token(kid: Option<&str>, exp: i64, extra: &str) -> String {
    let header = match kid {
        Some(kid) => format!(r#"{{"alg":"HS256","kid":"{kid}"}}"#),
        None => r#"{"alg":"HS256"}"#.to_owned(),
    };
    let payload = format!(r#"{{"exp":{exp},"sub":"user-42"{extra}}}"#);
    sign(&header, &payload, SECRET)
}

/// Returns why `plugin` refused `token` at [`now`].
fn refusal(plugin: &JwtPlugin, token: &str) -> &'static str {
    match plugin.authenticate(token, now()) {
        Ok(subject) => panic!("expected a refusal for {token}, got subject {subject}"),
        Err(detail) => detail,
    }
}

/// Builds a request context for the fixture document's route.
fn fresh() -> TestContext {
    TestContext::new("jwt").for_route(ROUTE)
}

/// Runs the request stage, mapping a refusal the way the data plane does.
async fn call(plugin: &JwtPlugin, ctx: &mut TestContext) -> Result<(), HttpError> {
    match plugin.on_request(ctx).await {
        Ok(Outcome::Continue) => Ok(()),
        Ok(other) => panic!("the jwt kind never answers instead of forwarding: {other:?}"),
        Err(error) => Err(HttpError::from(error)),
    }
}

mod acceptance {
    use super::*;

    #[test]
    fn a_valid_token_yields_its_subject() {
        assert_eq!(
            one_key().authenticate(&token(Some("k1"), NOW as i64 + 60, ""), now()),
            Ok("user-42".to_owned())
        );
    }

    #[test]
    fn a_token_without_a_kid_is_accepted_by_a_single_key_set() {
        assert!(
            one_key()
                .authenticate(&token(None, NOW as i64 + 60, ""), now())
                .is_ok()
        );
    }

    #[test]
    fn a_multi_key_set_accepts_a_token_signed_by_either_key() {
        let filter = filter(&entry_with_keys(""), &[SECRET, b"the-second-secret"]);

        assert_eq!(
            filter.authenticate(&token(Some("k1"), NOW as i64 + 60, ""), now()),
            Ok("user-42".to_owned())
        );
        let second = sign(
            r#"{"alg":"HS256","kid":"k2"}"#,
            &format!(r#"{{"exp":{},"sub":"user-42"}}"#, NOW + 60),
            b"the-second-secret",
        );
        assert_eq!(
            filter.authenticate(&second, now()),
            Ok("user-42".to_owned())
        );
    }

    #[test]
    fn a_token_inside_the_leeway_is_still_accepted() {
        // The default leeway is 30 s, so a token that expired ten seconds ago still verifies: a
        // verifier with a slightly ahead clock must not refuse a fresh token.
        let expired_ten = token(Some("k1"), NOW as i64 - 10, "");

        assert_eq!(
            one_key().authenticate(&expired_ten, now()),
            Ok("user-42".to_owned())
        );
    }

    #[test]
    fn a_configured_leeway_replaces_the_default() {
        let filter = filter(&entry_with("      leeway_seconds: 5\n"), &[SECRET]);

        assert_eq!(
            refusal(&filter, &token(Some("k1"), NOW as i64 - 10, "")),
            "the token has expired"
        );
    }

    #[test]
    fn a_matching_issuer_and_audience_are_required_and_accepted() {
        let filter = filter(
            &entry_with("      issuer: https://id.example\n      audience: api\n"),
            &[SECRET],
        );

        let good = token(
            Some("k1"),
            NOW as i64 + 60,
            r#","iss":"https://id.example","aud":["other","api"]"#,
        );
        assert_eq!(filter.authenticate(&good, now()), Ok("user-42".to_owned()));

        let wrong_issuer = token(
            Some("k1"),
            NOW as i64 + 60,
            r#","iss":"https://elsewhere.example","aud":"api""#,
        );
        assert_eq!(
            refusal(&filter, &wrong_issuer),
            "the token issuer does not match"
        );

        let wrong_audience = token(
            Some("k1"),
            NOW as i64 + 60,
            r#","iss":"https://id.example","aud":["other"]"#,
        );
        assert_eq!(
            refusal(&filter, &wrong_audience),
            "the token audience does not match"
        );
    }
}

mod refusals;

mod through_the_filter {
    use super::*;

    #[tokio::test]
    async fn a_valid_token_sets_the_identity_and_names_the_instance() {
        let filter = one_key();
        let valid = token(None, unix_now() + 3600, "");
        let mut ctx = fresh().with_header("authorization", &format!("Bearer {valid}"));

        assert!(call(&filter, &mut ctx).await.is_ok());

        assert_eq!(ctx.identity(), Some("user-42"));
        assert_eq!(
            ctx.plugin_name(),
            "jwt",
            "the instance is the source of the identity, which is what the log reports"
        );
    }

    #[tokio::test]
    async fn a_missing_credential_is_auth_missing() {
        let filter = one_key();

        for header in [None, Some("Basic abc"), Some("Bearer "), Some("bearer")] {
            let mut ctx = fresh();
            if let Some(value) = header {
                ctx = ctx.with_header("authorization", value);
            }
            let error = call(&filter, &mut ctx)
                .await
                .expect_err("no bearer credential is presented");
            let HttpError::Rejected { rejection, .. } = error else {
                panic!("expected a compiled rejection, got {error:?}");
            };
            assert_eq!(rejection.code(), "auth_missing", "header {header:?}");
            assert!(ctx.identity().is_none());
        }
    }

    #[tokio::test]
    async fn every_unusable_token_is_the_same_refusal_code() {
        // Expired, forged, algorithm-substituted and key-mismatched tokens are indistinguishable to
        // the caller: distinguishing them would let it enumerate which keys exist.
        let filter = one_key();
        let expired = token(None, unix_now() - 3600, "");
        let forged = sign(
            r#"{"alg":"HS256"}"#,
            &format!(r#"{{"exp":{},"sub":"user-42"}}"#, unix_now() + 3600),
            b"another-secret",
        );

        for token in [expired, forged, "not-a-token".to_owned()] {
            let mut ctx = fresh().with_header("authorization", &format!("Bearer {token}"));
            let error = call(&filter, &mut ctx)
                .await
                .expect_err("the token is unusable");
            let HttpError::Rejected { rejection, .. } = &error else {
                panic!("expected a compiled rejection, got {error:?}");
            };
            assert_eq!(rejection.code(), "auth_invalid");
            assert_eq!(
                error.category(),
                nico_core::ErrorCategory::Client,
                "an authentication failure is the caller's mistake"
            );
            assert!(
                error.respond().expect("renderable").challenge().is_some(),
                "every authentication refusal tells the caller how to authenticate"
            );
        }
    }

    #[tokio::test]
    async fn no_refusal_quotes_the_token() {
        let filter = one_key();
        let expired = token(None, unix_now() - 3600, "");
        let forged = sign(
            r#"{"alg":"HS256"}"#,
            &format!(r#"{{"exp":{},"sub":"user-42"}}"#, unix_now() + 3600),
            b"another-secret",
        );

        for token in [expired, forged, "malformed.token.value".to_owned()] {
            let mut ctx = fresh().with_header("authorization", &format!("Bearer {token}"));
            let error = call(&filter, &mut ctx)
                .await
                .expect_err("none of these is accepted");
            let rendered = format!("{error} {error:?}");
            assert!(
                !rendered.contains(&token),
                "a refusal must not repeat the credential: {rendered}"
            );
            assert!(
                !rendered.contains("user-42"),
                "no claim text either: {rendered}"
            );
        }
    }
}

/// Returns the wall clock as whole Unix seconds, for tokens the filter's own clock reads.
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

mod property;
