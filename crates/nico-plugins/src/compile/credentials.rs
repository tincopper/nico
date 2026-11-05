//! Materializing the credentials one plugin instance needs.
//!
//! Reading a credential is the only I/O a publication performs for a plugin, and it is kept apart from
//! both the plan compiler and each kind's factory for three reasons: the failure has its own shape
//! ([`PluginError::SecretRef`]), the secret itself must never leave this module, and a factory's job —
//! reading one kind's configuration — is easier to read without the file handling in the middle of it.
//!
//! What a reference *is* (`env:`/`file:`) is the document model's rule and is not re-decided here; this
//! module decides only how to read one, and refuses anything a filter could not use. A failure refuses
//! the whole document: a partially materialized key set would accept fewer callers than the operator
//! wrote.
//!
//! [`PluginError::SecretRef`]: crate::PluginError::SecretRef

use std::{
    env,
    fs::File,
    io::{self, Read},
    path::Path,
    sync::Arc,
};

use crate::{PluginError, spec::MAX_SECRET_BYTES};

/// Why a reference could not be materialized, as the sentence an operator reads.
///
/// These are fixed sentences rather than the underlying error's text: the cause of an `env` failure can
/// quote the value it read, so only a message this crate wrote may be rendered.
const NOT_SET: &str = "the environment variable is not set";
const NOT_UTF8: &str = "the value is not valid UTF-8";
const EMPTY: &str = "the credential is empty";
const TOO_LARGE: &str = "the credential is larger than the accepted bound";
const UNREADABLE: &str = "the credential cannot be read";
const NO_SOURCE: &str = "the reference names no credential source this build reads";

/// Reads one credential per reference, in the order the entry declares them.
///
/// # Errors
/// Returns the first [`PluginError::SecretRef`] a reference produces.
pub(crate) fn materialize_all<'a>(
    plugin: &str,
    references: impl Iterator<Item = &'a str>,
) -> Result<Vec<Arc<[u8]>>, PluginError> {
    references
        .map(|reference| materialize(plugin, reference))
        .collect()
}

/// Reads one credential reference into the value a plugin compares against.
///
/// # Errors
/// Returns [`PluginError::SecretRef`] naming the plugin, the reference and a fixed sentence describing
/// the failure when the environment variable is unset or not UTF-8, the file cannot be read or exceeds
/// [`MAX_SECRET_BYTES`], or the value is empty. Neither the bytes nor the underlying error's text is
/// ever part of the message: a `VarError` for a non-UTF-8 value quotes the value, which is exactly what
/// must not reach a log.
fn materialize(plugin: &str, reference: &str) -> Result<Arc<[u8]>, PluginError> {
    let failed = |cause: &'static str, source: Box<dyn std::error::Error + Send + Sync>| {
        PluginError::SecretRef {
            plugin: plugin.to_owned(),
            reference: reference.to_owned(),
            cause: cause.into(),
            source,
        }
    };

    if let Some(name) = reference.strip_prefix("env:") {
        let value = env::var(name).map_err(|error| match error {
            // `VarError`'s own message for a non-UTF-8 value quotes the value, which is the one thing
            // that must never reach a log; the cause is replaced with a fixed sentence instead.
            env::VarError::NotUnicode(_) => failed(NOT_UTF8, Box::new(not_utf8())),
            env::VarError::NotPresent => failed(NOT_SET, Box::new(error)),
        })?;
        return non_empty(value.into_bytes())
            .map(Arc::from)
            .map_err(|error| failed(EMPTY, Box::new(error)));
    }

    let Some(path) = reference.strip_prefix("file:") else {
        // The document model validates the scheme, so an unknown one means the model changed under
        // this crate: refuse rather than guess which source was meant.
        return Err(failed(
            NO_SOURCE,
            Box::new(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the reference names no credential source this build reads",
            )),
        ));
    };
    read_file(Path::new(path))
        .and_then(non_empty)
        .map(Arc::from)
        .map_err(|error| match error.kind() {
            io::ErrorKind::InvalidData => failed(TOO_LARGE, Box::new(error)),
            io::ErrorKind::InvalidInput => failed(EMPTY, Box::new(error)),
            _ => failed(UNREADABLE, Box::new(error)),
        })
}

/// Reads a `file:` credential, bounded by [`MAX_SECRET_BYTES`].
///
/// The bound is applied *by* the read rather than after it, so a reference pointing at a whole file
/// does not load it into a process that would then keep it for the generation's lifetime. The bytes
/// are the credential exactly as written: a trailing newline is part of the value, so a secret
/// written with `echo` is a different key — a mistake the first verification reports, rather than one
/// this compiler silently repairs by trimming a byte out of a credential.
///
/// # Errors
/// Returns the open or read failure, or [`io::ErrorKind::InvalidData`] when the value is larger than
/// the bound.
fn read_file(path: &Path) -> io::Result<Vec<u8>> {
    let limit = u64::try_from(MAX_SECRET_BYTES).unwrap_or(u64::MAX);
    let mut bytes = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_SECRET_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the credential exceeds {MAX_SECRET_BYTES} bytes"),
        ));
    }
    Ok(bytes)
}

/// Refuses an empty credential.
///
/// An empty key is a misconfiguration, not a key: it would make every token signed with an empty
/// secret verify, and every request presenting an empty API key authenticate. Refusing it at
/// publication is the only place the difference is still visible.
///
/// # Errors
/// Returns [`io::ErrorKind::InvalidInput`] when `bytes` is empty.
fn non_empty(bytes: Vec<u8>) -> io::Result<Vec<u8>> {
    if bytes.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the credential is empty",
        ));
    }
    Ok(bytes)
}

/// Builds the failure for an environment variable that is not valid UTF-8.
fn not_utf8() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "the value is not valid UTF-8")
}
