//! An HS256 signer for the plugin scenarios, and the credential files they materialize.
//!
//! The scenarios assert on the wire, so the token they send has to be a real one: a fixture that
//! hand-wrote the base64 segments would be a second implementation of the format under test, and the
//! first thing it would stop being is correct.
//!
//! Only the two knobs the scenarios vary are exposed — how long the token lives and who it names —
//! because anything else (issuer, audience, `nbf`) is a rule the crate's own unit tests already cover
//! with a clock they own.

use std::path::Path;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// The `kid` every fixture key answers to.
///
/// The document fixtures declare one key named `k1`; a token that names it exercises the key-selection
/// path a key set exists for, while a single-key entry would also accept a token that names nothing.
pub(crate) const KID: &str = "k1";

/// Signs a token for `subject` that expires `expires_in` seconds from now.
///
/// A negative value is how the scenarios build an expired token: the signature is valid, so the
/// refusal can only come from the claim rules.
pub(crate) fn token(secret: &str, expires_in: i64, subject: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the fixture clock is after the epoch")
        .as_secs();
    let expires = i64::try_from(now).expect("the fixture clock fits an i64") + expires_in;
    let header = format!(r#"{{"alg":"HS256","kid":"{KID}"}}"#);
    let payload = format!(r#"{{"exp":{expires},"sub":"{subject}"}}"#);
    sign(&header, &payload, secret)
}

/// Writes `contents` to `name` under `directory` and returns the `file:` reference naming it.
///
/// A file reference is what makes a credential materializable by a test at all: `env:` would need the
/// test to mutate the process environment, which is unsafe in the 2024 edition and shared by every
/// test in the binary.
pub(crate) fn secret_file(directory: &Path, name: &str, contents: &str) -> String {
    let path = directory.join(name);
    std::fs::write(&path, contents).expect("the credential fixture is written");
    format!("file:{}", path.display())
}

/// Signs `header` and `payload` with `secret`.
fn sign(header: &str, payload: &str, secret: &str) -> String {
    let header = URL_SAFE_NO_PAD.encode(header);
    let payload = URL_SAFE_NO_PAD.encode(payload);
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(format!("{header}.{payload}").as_bytes());
    let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    format!("{header}.{payload}.{signature}")
}
