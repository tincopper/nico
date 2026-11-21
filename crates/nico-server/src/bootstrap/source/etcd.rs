//! The `config_source.type: etcd` body: one key, its endpoints, and the transport they are reached
//! over.
//!
//! Everything this file accepts is a fact the process will live with until it is restarted: the
//! endpoints, the trust root that replaces the platform store, the client identity etcd may demand,
//! and the reference to the password it checks. Nothing here is re-read on a reload, and no reload
//! can widen or narrow it.
//!
//! Two invariants shape the accepted set:
//!
//! - **The transport is stated once, and completely.** An `https://` endpoint without a `tls` block,
//!   a `tls` block over a plaintext endpoint, and a list mixing the two are all startup errors.
//!   Guessing a scheme would either put the configuration document on the wire in clear text or
//!   fail to connect; both are worse than refusing to start.
//! - **This file holds paths, never material.** The trust root and the identity are opened here so a
//!   typo fails at startup, but only their paths are kept — the bytes are read again by the
//!   construction site, so a file swapped in between is a startup failure rather than a surprise on
//!   the first request. The password is not read here at all; its reference is, and it is resolved
//!   when the client is built.
//!
//! The two blocks are closed shapes of their own and `deny_unknown_fields` applies to both, so a
//! field written for `local` (or misspelt inside `tls`) is a parse error rather than an inert line.

use std::{
    borrow::Cow,
    path::{Path, PathBuf},
};

use nico_core::{is_secret_ref, validate_host};
use nico_http::{validate_bundle, validate_server_identity};
use serde::{Deserialize, Deserializer};

use super::SourceSelection;
use crate::bootstrap::BootstrapError;

mod endpoints;

/// The `etcd` body exactly as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawEtcdSource {
    #[serde(rename = "type")]
    _kind: String,
    endpoints: Vec<String>,
    key: String,
    max_bytes: usize,
    #[serde(default, deserialize_with = "block")]
    tls: Option<RawTls>,
    #[serde(default, deserialize_with = "block")]
    auth: Option<RawAuth>,
}

/// The `config_source.tls` block exactly as written.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTls {
    ca_file: Option<PathBuf>,
    cert_file: Option<PathBuf>,
    key_file: Option<PathBuf>,
    domain_name: Option<String>,
}

/// The `config_source.auth` block exactly as written.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAuth {
    user: Option<String>,
    password_ref: Option<String>,
}

/// Reads a block when it is present, including when its body is empty.
///
/// A block with no body (`tls:` and nothing under it) deserializes as `None`, which would be
/// indistinguishable from an omitted block — and an operator who wrote the key believes the
/// transport or the credentials are configured. Mapping the empty body to an empty block makes the
/// resolver report the missing field instead of silently keeping the plaintext, anonymous shape.
///
/// # Errors
/// Returns the deserializer's error when the block is not a mapping, or has unknown fields.
fn block<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.or(Some(T::default())))
}

/// Resolves the etcd body into the selection and the document size budget.
///
/// # Errors
/// Returns [`BootstrapError::InvalidField`] for a `tls` or `auth` block that cannot be used, for an
/// endpoint list whose schemes disagree with the transport the operator stated, or for a key name
/// that is not one. A block is resolved before the endpoints so a written-but-incomplete `tls` block
/// names its own field rather than the endpoints that depend on it.
pub(super) fn resolve(raw: RawEtcdSource) -> Result<(SourceSelection, usize), BootstrapError> {
    let tls = raw.tls.as_ref().map(RawTls::resolve).transpose()?;
    let auth = raw.auth.as_ref().map(RawAuth::resolve).transpose()?;
    endpoints::validate(&raw.endpoints, tls.is_some())?;
    validate_key(&raw.key)?;
    let selection = SourceSelection::Etcd {
        endpoints: raw.endpoints,
        key: raw.key,
        tls,
        auth,
    };
    Ok((selection, raw.max_bytes))
}

/// The transport one etcd connection uses: the trust root that replaces the platform store, the
/// identity etcd may require of this process, and the name the certificate is verified against.
///
/// The fields are **paths**. Neither the trust root nor the private key is carried by a validated
/// bootstrap, so the bytes of a key cannot leak through a value that gets cloned, compared or
/// printed.
///
/// The type is public because it is the payload of a public enum variant; its fields are not, so the
/// only way to read one is an accessor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EtcdTls {
    ca_file: PathBuf,
    identity: Option<(PathBuf, PathBuf)>,
    domain_name: Option<String>,
}

impl EtcdTls {
    /// Builds the block from the paths an operator wrote.
    pub(crate) const fn new(
        ca_file: PathBuf,
        identity: Option<(PathBuf, PathBuf)>,
        domain_name: Option<String>,
    ) -> Self {
        Self {
            ca_file,
            identity,
            domain_name,
        }
    }

    /// Returns the trust root every endpoint is verified against.
    pub(crate) fn ca_file(&self) -> &Path {
        &self.ca_file
    }

    /// Returns the client identity as a `(certificate chain, private key)` pair.
    pub(crate) fn identity(&self) -> Option<(&Path, &Path)> {
        self.identity
            .as_ref()
            .map(|(cert_file, key_file)| (cert_file.as_path(), key_file.as_path()))
    }

    /// Returns the name the presented certificate must carry, or `None` for the endpoint's host.
    pub(crate) fn domain_name(&self) -> Option<&str> {
        self.domain_name.as_deref()
    }
}

/// The user and password reference one etcd connection authenticates with.
///
/// The password is a reference (`env:`/`file:`), never a value: the bootstrap file is shipped with
/// the process, and the value it names is read once when the client is built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EtcdAuth {
    user: String,
    password_ref: String,
}

impl EtcdAuth {
    /// Builds the block from the user and the reference an operator wrote.
    pub(crate) const fn new(user: String, password_ref: String) -> Self {
        Self { user, password_ref }
    }

    /// Returns the user name etcd authenticates.
    pub(crate) fn user(&self) -> &str {
        &self.user
    }

    /// Returns the `env:` or `file:` reference the password is read from.
    pub(crate) fn password_ref(&self) -> &str {
        &self.password_ref
    }
}

impl RawTls {
    /// Opens what this block names and returns the transport it describes.
    ///
    /// The files are read here so that a missing or unusable one is a startup error naming its own
    /// field. Nothing is cached: the construction site reads them again, because a file that changed
    /// between the two reads must fail the process rather than the first request.
    ///
    /// # Errors
    /// Returns [`BootstrapError::InvalidField`] under `config_source.tls.*` when the block names no
    /// `ca_file`, when a half-written identity or an unusable `domain_name` is refused, and when a
    /// named file cannot be read or is not the PEM it claims to be. Neither a certificate nor a key
    /// byte reaches the message.
    fn resolve(&self) -> Result<EtcdTls, BootstrapError> {
        let Some(ca_file) = written(&self.ca_file) else {
            return Err(refuse(
                TLS,
                "ca_file",
                "is required when the `tls` block is present; etcd's trust root is never taken \
                 from the platform store",
            ));
        };
        let bytes = read(&ca_file, TLS, "ca_file")?;
        // The refusals are the data plane's own wording: it is the crate that knows which parse the
        // TLS client performs, and its message names what to fix.
        validate_bundle(&bytes).map_err(|error| {
            refuse(
                TLS,
                "ca_file",
                format!("is not a usable trust bundle: {error}"),
            )
        })?;

        let cert_file = written(&self.cert_file);
        let key_file = written(&self.key_file);
        // A pair is the unit: one half alone is a startup error, reported under `key_file` because
        // that is the half which completes a written `cert_file`.
        let identity = match (cert_file, key_file) {
            (None, None) => None,
            (Some(cert_file), Some(key_file)) => Some((cert_file, key_file)),
            _ => {
                return Err(refuse(
                    TLS,
                    "key_file",
                    "must be written together with `cert_file`",
                ));
            }
        };
        if let Some((cert_file, key_file)) = &identity {
            let certificate = read(cert_file, TLS, "cert_file")?;
            let key = read(key_file, TLS, "key_file")?;
            // `validate_server_identity` checks one fact — a certificate chain and a private key that
            // form a signing identity. The `server` in its name is the certificate's *direction* (a
            // server presents it), not who owns the block: this process presents this pair to etcd.
            validate_server_identity(&certificate, &key).map_err(|error| {
                refuse(
                    TLS,
                    error.field(),
                    format!("is not a usable TLS identity: {error}"),
                )
            })?;
        }

        let domain_name = self.resolve_domain_name()?;
        Ok(EtcdTls::new(ca_file, identity, domain_name))
    }

    /// Returns the name every endpoint's certificate is verified against.
    ///
    /// # Errors
    /// Returns [`BootstrapError::InvalidField`] under `config_source.tls.domain_name` when the
    /// written name is not one a certificate can carry.
    fn resolve_domain_name(&self) -> Result<Option<String>, BootstrapError> {
        let Some(name) = self.domain_name.as_deref() else {
            return Ok(None);
        };
        validate_host(name).map_err(|error| BootstrapError::InvalidField {
            field: "config_source.tls.domain_name".into(),
            reason: error.reason(),
        })?;
        Ok(Some(name.to_owned()))
    }
}

impl RawAuth {
    /// Resolves the block into the credentials the client is built with.
    ///
    /// The reference is checked for *shape* only: reading it here would put the password into a
    /// value the bootstrap hands onward, and a bootstrap file is shipped with the process.
    ///
    /// # Errors
    /// Returns [`BootstrapError::InvalidField`] under `config_source.auth.user` or
    /// `config_source.auth.password_ref` when either is missing or not a reference this build reads.
    fn resolve(&self) -> Result<EtcdAuth, BootstrapError> {
        let Some(user) = self.user.as_deref().filter(|user| !user.is_empty()) else {
            return Err(refuse(
                AUTH,
                "user",
                "is required when the `auth` block is present",
            ));
        };
        if user.trim() != user {
            return Err(refuse(
                AUTH,
                "user",
                "must not contain surrounding whitespace",
            ));
        }
        let Some(password_ref) = self.password_ref.as_deref() else {
            return Err(refuse(
                AUTH,
                "password_ref",
                "is required when the `auth` block is present",
            ));
        };
        if !is_secret_ref(password_ref) {
            return Err(refuse(
                AUTH,
                "password_ref",
                "must be an `env:` or `file:` reference; the password is never written in the \
                 bootstrap file",
            ));
        }
        Ok(EtcdAuth::new(user.to_owned(), password_ref.to_owned()))
    }
}

/// Validates the watched key: a non-empty, unpadded key name.
fn validate_key(key: &str) -> Result<(), BootstrapError> {
    if key.trim() != key || key.is_empty() {
        return Err(BootstrapError::InvalidField {
            field: "config_source.key".into(),
            reason: "must be a non-empty key name without surrounding whitespace".into(),
        });
    }
    Ok(())
}

/// Returns the path an optional field names, treating an empty one as absent.
fn written(path: &Option<PathBuf>) -> Option<PathBuf> {
    path.as_ref()
        .filter(|path| !path.as_os_str().is_empty())
        .cloned()
}

/// Reads one PEM file a `tls` block names.
///
/// The read is bounded by [`MAX_TLS_MATERIAL_BYTES`]: a ceiling checked outside the read is not a
/// ceiling, and a mis-pointed path must not load an arbitrary file into the process.
fn read(path: &Path, block: &str, name: &'static str) -> Result<Vec<u8>, BootstrapError> {
    nico_config::read_bounded(path, MAX_TLS_MATERIAL_BYTES).map_err(|error| match error {
        nico_config::ReadError::TooLarge { .. } => {
            refuse(block, name, "is larger than the accepted bound")
        }
        nico_config::ReadError::Io(source) => {
            refuse(block, name, format!("could not be read: {source}"))
        }
        _ => refuse(block, name, format!("could not be read: {error}")),
    })
}

/// Builds the rejection for one field of a block: `config_source.<block>.<name>`.
fn refuse(block: &str, name: &'static str, reason: impl Into<Cow<'static, str>>) -> BootstrapError {
    BootstrapError::InvalidField {
        field: format!("config_source.{block}.{name}").into(),
        reason: reason.into(),
    }
}

/// Largest accepted TLS material file (`ca_file`, `cert_file`, `key_file`), in bytes.
///
/// Real trust chains and client identities fit many times over; the bound exists so a mis-pointed
/// path cannot load an arbitrary file into the process — the reason the password reference is
/// bounded too. The construction site re-applies the same value, so a file the bootstrap read
/// cannot fail the second read on size alone.
pub(crate) const MAX_TLS_MATERIAL_BYTES: usize = 1024 * 1024;

/// The dotted name of the transport block.
const TLS: &str = "tls";

/// The dotted name of the credentials block.
const AUTH: &str = "auth";
