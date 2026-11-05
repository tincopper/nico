//! HS256 JSON Web Token authentication.
//!
//! The order of the checks is the contract, because each one exists to stop something the next one
//! cannot:
//!
//! 1. the token's length, so nothing below works on an unbounded input;
//! 2. three base64url segments, so a token that is not a JWT is refused before any key is touched;
//! 3. `alg` must be exactly `HS256` — `none` and every other algorithm are refused *here*, before a
//!    key is selected, which is what makes an algorithm-substitution token useless;
//! 4. the key is selected by `kid` (required when the set holds more than one, optional but checked
//!    when it does not), so a token cannot choose a key the entry does not accept;
//! 5. the signature is compared in constant time;
//! 6. the claims are validated, with `exp` **required** — a token without an expiry is a permanent
//!    credential — plus `nbf`, `iss` and `aud` when they are configured;
//! 7. the subject becomes the request's identity.
//!
//! Every failure after "is there a credential at all" is the same refusal with the same stable code.
//! The cause is a fixed sentence that only reaches the log, so a caller cannot use the error code to
//! learn whether a key exists, whether a signature was close, or when a token expired.

mod config;
mod factory;
mod json;

// The kind's own configuration, reachable from the compiler because the document model hands it over
// as an opaque value: one path to it is easier to follow than two.
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
pub(crate) use config::JwtConfig;
pub(crate) use factory::JwtFactory;
use hmac::{Hmac, Mac};
use nico_http::{Outcome, Plugin, PluginContext, PluginError, PluginInfo, PluginResult, Rejection};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::{
    rejection::AuthRejections,
    spec::{JwtKeyRef, JwtSpec, MAX_SUBJECT_BYTES, MAX_TOKEN_BYTES},
};

/// The only algorithm this build verifies.
const ALGORITHM: &str = "HS256";

/// The request header a bearer credential arrives in.
const AUTHORIZATION: &str = "authorization";

/// A minimal configuration this kind accepts, as JSON text.
///
/// `jwt` has no defaults — a token cannot be verified against a key set nobody wrote — so this is a
/// complete one-key example rather than an empty mapping. [`INFO`] and the factory report the same
/// text, because there is one copy of it.
pub(crate) const DEFAULT_CONFIG: &str = r#"{"keys":[{"kid":"k1","secret_ref":"env:JWT_SECRET"}]}"#;

/// What an operator can learn about this kind before enabling it.
pub(crate) const INFO: PluginInfo = PluginInfo::new(
    env!("CARGO_PKG_VERSION"),
    "HS256 bearer-token authentication",
    DEFAULT_CONFIG,
);

/// Why a request carried no credential.
const NO_CREDENTIAL: &str = "the request carries no bearer credential";
/// Why a token could not be used, when the cause is its shape.
const MALFORMED: &str = "the token is not a well-formed JWT";
/// Why a token could not be used, when it is longer than any accepted token.
const TOO_LARGE: &str = "the token is too large to be accepted";
/// Why a token could not be used, when it names another algorithm.
const NOT_HS256: &str = "the token does not use HS256";
/// Why a token could not be used, when no accepted key answers to its `kid`.
const NO_KEY: &str = "the token names no key this plugin accepts";
/// Why a token could not be used, when its signature does not verify.
const BAD_SIGNATURE: &str = "the token signature does not verify";
/// Why a token could not be used, when it carries no usable expiry.
const NO_EXPIRY: &str = "the token carries no expiry";
/// Why a token could not be used, when it has expired.
const EXPIRED: &str = "the token has expired";
/// Why a token could not be used, when it is not valid yet.
const NOT_YET_VALID: &str = "the token is not valid yet";
/// Why a token could not be used, when its issuer is not the configured one.
const WRONG_ISSUER: &str = "the token issuer does not match";
/// Why a token could not be used, when its audience is not the configured one.
const WRONG_AUDIENCE: &str = "the token audience does not match";
/// Why a token could not be used, when it does not name a subject.
const NO_SUBJECT: &str = "the token carries no subject";
/// Why a token could not be used, when its subject is longer than an identity may be.
const SUBJECT_TOO_LARGE: &str = "the token subject is too large";

/// The HMAC construction this build verifies.
type HmacSha256 = Hmac<Sha256>;

/// One accepted key: the `kid` a token may name and the shared secret it was signed with.
struct Key {
    kid: Option<Arc<str>>,
    secret: Arc<[u8]>,
}

/// One `jwt` policy instance, shared by every request of every route that references it.
pub(crate) struct JwtPlugin {
    /// The plugin instance name: it becomes the identity's `source` and the `authn` log field.
    instance: Arc<str>,
    keys: Vec<Key>,
    issuer: Option<Arc<str>>,
    audience: Option<Arc<str>>,
    leeway: Duration,
    rejections: AuthRejections,
}

impl JwtPlugin {
    /// Builds one filter from its compiled entry and the materialized secrets, in key order.
    ///
    /// The secrets are passed in rather than read here because reading a credential is I/O, and it
    /// happens while the document is compiled — once per publication, never on the request path.
    ///
    /// # Errors
    /// Returns [`PluginError::CredentialCountMismatch`] when `secrets` does not hold one value per
    /// declared key: padding or truncating would silently change which tokens are accepted.
    pub(crate) fn new(spec: &JwtSpec, secrets: Vec<Arc<[u8]>>) -> Result<Self, crate::PluginError> {
        if secrets.len() != spec.keys.len() {
            return Err(crate::PluginError::CredentialCountMismatch {
                plugin: spec.instance.to_string(),
                declared: spec.keys.len(),
                materialized: secrets.len(),
            });
        }
        Ok(Self {
            instance: Arc::clone(&spec.instance),
            keys: spec
                .keys
                .iter()
                .zip(secrets)
                .map(|(key, secret)| Key {
                    kid: key.kid.as_ref().map(Arc::clone),
                    secret,
                })
                .collect(),
            issuer: spec.issuer.as_ref().map(Arc::clone),
            audience: spec.audience.as_ref().map(Arc::clone),
            leeway: spec.leeway,
            rejections: spec.rejections.clone(),
        })
    }

    /// Verifies one bearer token, returning the subject it authenticates.
    ///
    /// `now` is a parameter so the claim rules can be tested against a clock the test owns, instead of
    /// the test racing the wall clock.
    ///
    /// # Errors
    /// Returns the fixed sentence describing why the token was refused. It never quotes the token or
    /// any part of it, so it is safe to log.
    pub(crate) fn authenticate(
        &self,
        token: &str,
        now: SystemTime,
    ) -> Result<String, &'static str> {
        if token.len() > MAX_TOKEN_BYTES {
            return Err(TOO_LARGE);
        }
        let (header_segment, payload_segment, signature_segment) = segments(token)?;
        let header = decode(header_segment)?;
        let payload = decode(payload_segment)?;
        let signature = decode(signature_segment)?;

        let header = json::parse(&header).map_err(|_error| MALFORMED)?;
        // The algorithm is checked before a key is selected: a token that says `none` — or names the
        // key of a verifier this build does not have — must not reach key selection at all.
        if header.get("alg").and_then(json::Value::as_str) != Some(ALGORITHM) {
            return Err(NOT_HS256);
        }
        let kid = match header.get("kid") {
            Some(kid) => Some(kid.as_str().ok_or(MALFORMED)?),
            None => None,
        };

        let key = self.select(kid)?;
        let signed = signed_bytes(token, header_segment, payload_segment)?;
        verify(key, signed, &signature)?;

        let claims = json::parse(&payload).map_err(|_error| MALFORMED)?;
        self.subject(&claims, now)
    }

    /// Selects the key a token names.
    ///
    /// A single-key entry may leave `kid` out, so a token that names one must still match it: accepting
    /// a token that names an unknown key would make rotation silently keep the old key alive.
    fn select(&self, kid: Option<&str>) -> Result<&Key, &'static str> {
        match (self.keys.as_slice(), kid) {
            ([key], None) => Ok(key),
            ([key], Some(kid)) => match key.kid.as_deref() {
                Some(expected) if expected == kid => Ok(key),
                _ => Err(NO_KEY),
            },
            (keys, Some(kid)) => keys
                .iter()
                .find(|key| key.kid.as_deref() == Some(kid))
                .ok_or(NO_KEY),
            (_, None) => Err(NO_KEY),
        }
    }

    /// Validates the claims the entry requires and returns the subject.
    ///
    /// `exp` is required: a token without an expiry can never be revoked by time, so accepting one
    /// would turn a short-lived credential into a permanent one.
    fn subject(&self, claims: &json::Value, now: SystemTime) -> Result<String, &'static str> {
        let expires = claims
            .get("exp")
            .and_then(json::Value::as_f64)
            .ok_or(NO_EXPIRY)?;
        let now = unix_seconds(now)?;
        let leeway = self.leeway.as_secs_f64();
        if now > expires + leeway {
            return Err(EXPIRED);
        }
        if let Some(not_before) = claims.get("nbf") {
            let not_before = not_before.as_f64().ok_or(MALFORMED)?;
            if now + leeway < not_before {
                return Err(NOT_YET_VALID);
            }
        }
        if let Some(issuer) = &self.issuer {
            let found = claims
                .get("iss")
                .and_then(json::Value::as_str)
                .ok_or(WRONG_ISSUER)?;
            if found != issuer.as_ref() {
                return Err(WRONG_ISSUER);
            }
        }
        if let Some(audience) = &self.audience {
            if !audience_matches(claims.get("aud"), audience) {
                return Err(WRONG_AUDIENCE);
            }
        }
        let subject = claims
            .get("sub")
            .and_then(json::Value::as_str)
            .ok_or(NO_SUBJECT)?;
        if subject.is_empty() {
            return Err(NO_SUBJECT);
        }
        if subject.len() > MAX_SUBJECT_BYTES {
            return Err(SUBJECT_TOO_LARGE);
        }
        Ok(subject.to_owned())
    }

    /// Builds the refusal `detail` describes.
    fn refuse(&self, detail: &'static str, rejection: Arc<Rejection>) -> PluginError {
        tracing::debug!(
            plugin = %self.instance,
            reason = detail,
            "the request was not authenticated"
        );
        PluginError::Refused {
            rejection,
            detail,
            retry_after: None,
        }
    }
}

#[async_trait]
impl Plugin for JwtPlugin {
    fn name(&self) -> &str {
        &self.instance
    }

    fn info(&self) -> PluginInfo {
        INFO
    }

    async fn on_request(&self, ctx: &mut dyn PluginContext) -> PluginResult {
        let presented = ctx.request_header(AUTHORIZATION)?;
        let Some(token) = presented.as_deref().and_then(bearer) else {
            return Err(self.refuse(NO_CREDENTIAL, self.rejections.missing()));
        };
        match self.authenticate(token, SystemTime::now()) {
            Ok(subject) => {
                // The context records this plugin's own instance name as the source, so a subject is
                // attributable without the plugin knowing how identity is stored.
                ctx.set_identity(&subject)?;
                Ok(Outcome::Continue)
            }
            Err(detail) => Err(self.refuse(detail, self.rejections.invalid())),
        }
    }
}

/// Splits a token into its three segments, refusing any other shape.
fn segments(token: &str) -> Result<(&str, &str, &str), &'static str> {
    let mut parts = token.split('.');
    let (Some(header), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(MALFORMED);
    };
    Ok((header, payload, signature))
}

/// Returns the bytes the signature covers: the header and payload segments, dots included.
///
/// It is computed from the segments rather than from a re-serialized header, because a signature
/// covers the exact bytes that were signed — whitespace and key order included.
///
/// # Errors
/// Returns [`MALFORMED`] when the two segments do not fit inside the token, which cannot happen for
/// segments [`segments`] produced; the check exists so this function is total without indexing that
/// could panic.
fn signed_bytes<'a>(token: &'a str, header: &str, payload: &str) -> Result<&'a [u8], &'static str> {
    let length = header.len() + 1 + payload.len();
    token.as_bytes().get(..length).ok_or(MALFORMED)
}

/// Decodes one base64url segment without padding.
fn decode(segment: &str) -> Result<Vec<u8>, &'static str> {
    URL_SAFE_NO_PAD.decode(segment).map_err(|_error| MALFORMED)
}

/// Verifies `signature` over `signed` with `key`, in constant time.
fn verify(key: &Key, signed: &[u8], signature: &[u8]) -> Result<(), &'static str> {
    let mut mac = HmacSha256::new_from_slice(&key.secret).map_err(|_error| BAD_SIGNATURE)?;
    mac.update(signed);
    let expected = mac.finalize().into_bytes();
    // `ct_eq` also compares the lengths, so a truncated signature cannot take a faster path.
    if bool::from(expected.as_slice().ct_eq(signature)) {
        return Ok(());
    }
    Err(BAD_SIGNATURE)
}

/// Returns the `Authorization` header's bearer token, when it presents one.
///
/// The scheme is case-insensitive (RFC 6750 §2.1). Anything that is not a bearer credential — another
/// scheme, an empty value, non-ASCII bytes — means the request presented none, which is a different
/// answer from "it presented one that is unusable".
fn bearer(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    if token.is_empty() || token.contains(' ') {
        return None;
    }
    Some(token)
}

/// Returns whether a token's `aud` claim contains the configured audience.
///
/// RFC 7519 allows either a string or an array of strings; a set of two is the common case for a
/// token that several services accept, and requiring the configured value to be one of them is what
/// keeps a token issued for another audience from being accepted here.
fn audience_matches(claim: Option<&json::Value>, audience: &str) -> bool {
    match claim {
        Some(json::Value::String(found)) => found == audience,
        Some(value) => value
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item.as_str() == Some(audience))),
        None => false,
    }
}

/// Returns the seconds since the Unix epoch, as the float a `NumericDate` is compared against.
fn unix_seconds(now: SystemTime) -> Result<f64, &'static str> {
    now.duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        // A clock before 1970 makes every `exp` in the future, so the token cannot be judged rather
        // than accepted: refusing is the fail-closed answer for a broken clock.
        .map_err(|_error| MALFORMED)
}

// Compiled-in proof that a filter and the refusal it shares can cross `.await` and threads.
const _: fn() = || {
    const fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<JwtPlugin>();
    assert_send_sync_static::<JwtKeyRef>();
};

#[cfg(test)]
mod tests;
