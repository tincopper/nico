//! Materializing the provider credential a document references.
//!
//! A document never carries a secret: `providers[].credential_ref` is an `env:`/`file:` reference the
//! document model already validated (`nico_core::is_secret_ref`). Reading it is the only I/O a
//! publication performs for an LLM route, and it happens **once per provider a route references**,
//! while the plan is compiled — never per request, and never for an entry nothing forwards to. A
//! request that read a file would make the process's key rotation a latency spike, and a missing key a
//! per-request failure instead of a refused document.
//!
//! What a reference *is* is the document model's rule and is not re-decided here; this module decides
//! only how to read one, and refuses anything an adapter could not send. A failure refuses the whole
//! document: publishing a route whose credential could not be read would make every one of its
//! requests answer `401` from the provider, which is the operator's mistake reported as the caller's.
//!
//! Two invariants shape the value:
//!
//! - **The bytes are the credential.** A `file:` reference is read byte for byte; a trailing newline
//!   written by `echo` is part of the key, so it is kept. Silently trimming it would make a
//!   misconfigured key work here and fail against every other client, which is the wrong place to be
//!   forgiving.
//! - **A failure never quotes a value.** Every cause is a fixed sentence this crate wrote, and the
//!   only variable part of the error is the reference itself — which is what the operator wrote, not
//!   what was read.

use std::{env, path::Path};

use nico_core::ErrorCategory;
use nico_llm::Credential;

/// Largest accepted credential, in bytes.
///
/// A credential larger than this is not a credential this build will send: the bound keeps a
/// mis-pointed `file:` reference from loading a whole file into a process that then holds it for the
/// lifetime of the generation. Provider keys are tens of bytes; the bound is generous on purpose.
const MAX_CREDENTIAL_BYTES: usize = 8 * 1024;

/// Reads the value a `credential_ref` names.
///
/// A trait rather than a free function so the plan compiler can be tested without touching the
/// process environment: a test that had to set an environment variable would be unsafe in the 2024
/// edition and would share it with every other test in the binary.
pub trait CredentialResolver: Send + Sync {
    /// Returns the credential `reference` names.
    ///
    /// # Errors
    /// Returns [`CredentialError`] when the reference is not one this build reads, or when the value
    /// it names cannot be read as a bounded, non-empty, UTF-8 credential.
    fn resolve(&self, reference: &str) -> Result<Credential, CredentialError>;
}

/// The resolver a process uses: `env:NAME` from the environment, `file:/path` from the filesystem.
///
/// It holds no state — neither source can be cached across publications, because a reload is exactly
/// when an operator expects a rotated key to be picked up — so one value serves the process.
#[derive(Debug, Clone, Copy, Default)]
pub struct EnvFileCredentials;

impl EnvFileCredentials {
    /// Creates the resolver.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl CredentialResolver for EnvFileCredentials {
    fn resolve(&self, reference: &str) -> Result<Credential, CredentialError> {
        if let Some(name) = reference.strip_prefix("env:") {
            if name.is_empty() {
                return Err(CredentialError::unusable(reference, NO_NAME));
            }
            let value = env::var(name).map_err(|error| match error {
                // `VarError`'s own message for a non-UTF-8 value quotes the value, which is the one
                // thing that must never reach a log; a fixed sentence replaces it.
                env::VarError::NotUnicode(_) => CredentialError::unreadable(reference, NOT_UTF8),
                env::VarError::NotPresent => CredentialError::unreadable(reference, NOT_SET),
            })?;
            check_value(reference, &value)?;
            return Ok(Credential::new(value));
        }

        let Some(path) = reference.strip_prefix("file:") else {
            // The document model validates the scheme, so an unknown one means the model changed
            // under this crate: refuse rather than guess which source was meant.
            return Err(CredentialError::unusable(reference, NO_SOURCE));
        };
        if path.is_empty() {
            return Err(CredentialError::unusable(reference, NO_NAME));
        }
        let bytes = read_file(Path::new(path), reference)?;
        let value = String::from_utf8(bytes)
            .map_err(|_| CredentialError::unreadable(reference, NOT_UTF8))?;
        check_value(reference, &value)?;
        Ok(Credential::new(value))
    }
}

/// Refuses a value this build will not send, whatever source produced it.
///
/// One place for the empty check and the bound, because the two sources differ in how they read and
/// not in what they accept: an `env:` and a `file:` reference to the same bytes must resolve the same
/// way, and a bound enforced on one branch is a bound the other can drift from silently.
fn check_value(reference: &str, value: &str) -> Result<(), CredentialError> {
    if value.is_empty() {
        return Err(CredentialError::unreadable(reference, EMPTY));
    }
    if value.len() > MAX_CREDENTIAL_BYTES {
        return Err(CredentialError::unreadable(reference, TOO_LARGE));
    }
    Ok(())
}

/// Why a reference could not be materialized.
///
/// The value carries the reference, a fixed cause and the category a refused publication is reported
/// as. It never carries the secret, the bytes read, or the underlying I/O error's text: a `VarError`
/// for a non-UTF-8 value quotes the value, and an I/O error may quote a path — the reference already
/// names the path, and nothing else about the read belongs in a log.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("credential reference `{reference}` is unusable: {cause}")]
pub struct CredentialError {
    reference: String,
    cause: &'static str,
    category: ErrorCategory,
}

impl CredentialError {
    /// The reference the document wrote, for example `env:OPENAI_API_KEY`.
    #[must_use]
    pub fn reference(&self) -> &str {
        &self.reference
    }

    /// The fixed sentence describing why it could not be used.
    #[must_use]
    pub const fn cause(&self) -> &'static str {
        self.cause
    }

    /// How a refused publication is classified.
    ///
    /// A reference this build does not read is the document's mistake ([`ErrorCategory::Client`]);
    /// a value that cannot be read is a dependency failure ([`ErrorCategory::Upstream`]), the same
    /// split the plugin compiler makes for its own credential references.
    #[must_use]
    pub const fn category(&self) -> ErrorCategory {
        self.category
    }

    /// Builds the failure for a reference this build cannot interpret.
    pub(crate) fn unusable(reference: &str, cause: &'static str) -> Self {
        Self {
            reference: reference.to_owned(),
            cause,
            category: ErrorCategory::Client,
        }
    }

    /// Builds the failure for a reference whose value could not be read.
    pub(crate) fn unreadable(reference: &str, cause: &'static str) -> Self {
        Self {
            reference: reference.to_owned(),
            cause,
            category: ErrorCategory::Upstream,
        }
    }
}

/// The reference names no source this build reads.
const NO_SOURCE: &str = "the reference must begin with `env:` or `file:`";

/// The reference names neither a variable nor a path.
const NO_NAME: &str = "the reference names nothing";

/// The environment variable is not set.
const NOT_SET: &str = "the environment variable is not set";

/// The value is not valid UTF-8.
const NOT_UTF8: &str = "the value is not valid UTF-8";

/// The value is empty.
const EMPTY: &str = "the credential is empty";

/// The value is larger than [`MAX_CREDENTIAL_BYTES`].
const TOO_LARGE: &str = "the credential is larger than the accepted bound";

/// The file cannot be read.
const UNREADABLE: &str = "the credential cannot be read";

/// Reads a `file:` credential through the shared bounded reader.
///
/// The ceiling is applied by the read, not after it, and the rule lives with the reader rather than
/// here: a credential file and a configuration document differ in what they hold, not in how much of
/// it the process is willing to load. Every failure maps to a fixed sentence, because the underlying
/// cause can quote a path.
fn read_file(path: &Path, reference: &str) -> Result<Vec<u8>, CredentialError> {
    nico_config::read_bounded(path, MAX_CREDENTIAL_BYTES).map_err(|error| match error {
        nico_config::ReadError::TooLarge { .. } => {
            CredentialError::unreadable(reference, TOO_LARGE)
        }
        _ => CredentialError::unreadable(reference, UNREADABLE),
    })
}

// Compiled-in proof that a refused credential can cross `.await` and `tokio::spawn` boundaries.
const _: fn() = || {
    const fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<CredentialError>();
};

#[cfg(test)]
mod tests;
