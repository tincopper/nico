//! Assembling an etcd client from a validated bootstrap: the material, the credentials, the thread.
//!
//! The bootstrap holds paths and references, so this module is where they become bytes the client
//! can present. Each read repeats the check the bootstrap performed, for the same reason an inbound
//! listener repeats it: the two reads are two syscalls on a name, and a file replaced in between
//! must be a startup error naming its field rather than a handshake failure on the first request.
//!
//! Nothing here reads the *platform* trust store. etcd's trust root is `config_source.tls.ca_file`
//! and nothing else, so "which server is this" is a fact of the bootstrap file rather than of the
//! host the process happens to run on.
//!
//! Configured credentials make reachability a startup fact too: the client exchanges them for a
//! token while it connects, so a refused password or an unreachable endpoint fails this build
//! instead of the first read. That is the intended shape — a bootstrap that meant to authenticate
//! must not keep serving because the server never got the chance to refuse it.
//!
//! A failure never quotes what it read: the field name is the operator's own text, and the password
//! and the key bytes have no spelling in an error.

use std::{borrow::Cow, env, path::Path, sync::mpsc};

use nico_config_etcd::{
    EtcdConnectOptions, EtcdCredentials, EtcdError, EtcdTlsOptions, GrpcEtcdClient,
};
use nico_http::{validate_bundle, validate_server_identity};

use super::SourceBuildError;
use crate::bootstrap::{EtcdAuth, EtcdTls, MAX_TLS_MATERIAL_BYTES};

/// Largest accepted etcd password, in bytes.
///
/// A password larger than this is not a password: the bound keeps a mis-pointed reference from
/// loading an arbitrary value into a process that then holds it for its lifetime. It matches the
/// bound the resource-credential reader applies, because the two are read the same way, and it
/// bounds both sources: `file:` through its read, `env:` through [`accepted`], the rule both
/// values end at.
const MAX_PASSWORD_BYTES: usize = 8 * 1024;

/// Builds the connection options the client is created with.
///
/// # Errors
/// Returns [`SourceBuildError::Setting`] naming the field when a file the bootstrap named cannot be
/// read or is no longer usable, or when the password reference cannot be resolved.
pub(super) fn connect_options(
    tls: Option<&EtcdTls>,
    auth: Option<&EtcdAuth>,
) -> Result<EtcdConnectOptions, SourceBuildError> {
    let mut options = EtcdConnectOptions::new();
    if let Some(tls) = tls {
        options = options.with_tls(tls_options(tls)?);
    }
    if let Some(auth) = auth {
        let password = resolve(auth.password_ref())?;
        options = options.with_credentials(EtcdCredentials::new(auth.user(), password));
    }
    Ok(options)
}

/// Connects the etcd client on the runtime that will own its transports forever.
///
/// The thread is detached on purpose: the runtime must not be dropped while the client lives, and
/// the process's shutdown protocol stays the single cancellation token — this thread does not
/// outlive the process it serves.
///
/// # Errors
/// Returns [`EtcdError::Connect`] when the runtime or its thread cannot start, or when the client's
/// own connect fails.
pub(super) fn connect_on_its_own_runtime(
    endpoints: Vec<String>,
    options: EtcdConnectOptions,
) -> Result<GrpcEtcdClient, EtcdError> {
    let (sender, receiver) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("etcd-client".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = sender.send(Err(EtcdError::Connect {
                        message: format!("the client runtime could not start: {error}"),
                    }));
                    return;
                }
            };
            let connected = runtime.block_on(GrpcEtcdClient::connect(&endpoints, options));
            // The client is only delivered once the runtime that owns its transports is running
            // for good; everything after this send exists to keep that runtime alive.
            if sender.send(connected).is_err() {
                return;
            }
            runtime.block_on(std::future::pending::<()>());
        });
    spawned.map_err(|error| EtcdError::Connect {
        message: format!("the client runtime thread could not start: {error}"),
    })?;
    receiver.recv().map_err(|_| EtcdError::Connect {
        message: "the client runtime thread ended before the client was delivered".into(),
    })?
}

/// Reads and re-checks the trust material a `tls` block names.
///
/// # Errors
/// Returns [`SourceBuildError::Setting`] under `config_source.tls.*` when a named file cannot be
/// read, is no longer a trust bundle, or no longer pairs the certificate with its key.
fn tls_options(tls: &EtcdTls) -> Result<EtcdTlsOptions, SourceBuildError> {
    let ca_pem = read_file(tls.ca_file(), CA_FILE)?;
    // The same check the bootstrap ran, on the bytes the client will actually use: a file swapped
    // between the two reads must fail here, where the field name is known.
    validate_bundle(&ca_pem)
        .map_err(|error| setting(CA_FILE, format!("is not a usable trust bundle: {error}")))?;
    let mut options = EtcdTlsOptions::new(ca_pem);

    if let Some((cert_file, key_file)) = tls.identity() {
        let certificate = read_file(cert_file, CERT_FILE)?;
        let key = read_file(key_file, KEY_FILE)?;
        // `validate_server_identity` checks that one chain and one key form a signing identity. The
        // `server` in its name is the certificate's direction, not who owns the field: this process
        // presents the pair to etcd, exactly as it would present one to a client.
        validate_server_identity(&certificate, &key).map_err(|error| {
            let field = if error.field() == "cert_file" {
                CERT_FILE
            } else {
                KEY_FILE
            };
            setting(field, format!("is not a usable TLS identity: {error}"))
        })?;
        options = options.with_identity(certificate, key);
    }

    if let Some(domain_name) = tls.domain_name() {
        options = options.with_domain_name(domain_name);
    }
    Ok(options)
}

/// Resolves the password an `auth` block references.
///
/// A `file:` reference is read byte for byte; a trailing newline written by `echo` is part of the
/// password, the same convention every other credential in this process follows. Trimming it would
/// make a bootstrap file work here and fail against every other etcd client.
///
/// # Errors
/// Returns [`SourceBuildError::Setting`] under `config_source.auth.password_ref` when the reference
/// names no variable or path, when the value cannot be read as UTF-8, or when it is empty or
/// oversized.
fn resolve(reference: &str) -> Result<String, SourceBuildError> {
    if let Some(name) = reference.strip_prefix("env:") {
        if name.is_empty() {
            return Err(setting(PASSWORD_REF, "the reference names nothing"));
        }
        let value = env::var(name).map_err(|error| match error {
            // `VarError`'s own message for a non-UTF-8 value quotes the value, which is the one
            // thing that must never reach a log; a fixed sentence replaces it.
            env::VarError::NotUnicode(_) => {
                setting(PASSWORD_REF, "the environment variable is not valid UTF-8")
            }
            env::VarError::NotPresent => {
                setting(PASSWORD_REF, "the environment variable is not set")
            }
        })?;
        return accepted(value);
    }

    let Some(path) = reference.strip_prefix("file:") else {
        // The bootstrap validates the scheme, so an unknown one means the section changed under
        // this crate: refuse rather than guess which source was meant.
        return Err(setting(
            PASSWORD_REF,
            "the reference must begin with `env:` or `file:`",
        ));
    };
    if path.is_empty() {
        return Err(setting(PASSWORD_REF, "the reference names nothing"));
    }
    let bytes = nico_config::read_bounded(Path::new(path), MAX_PASSWORD_BYTES).map_err(
        |error| match error {
            nico_config::ReadError::TooLarge { .. } => setting(
                PASSWORD_REF,
                "the password is larger than the accepted bound",
            ),
            _ => setting(PASSWORD_REF, "the password cannot be read"),
        },
    )?;
    let value = String::from_utf8(bytes)
        .map_err(|_| setting(PASSWORD_REF, "the password is not valid UTF-8"))?;
    accepted(value)
}

/// Returns the value this build will send, refusing an empty or oversized one.
///
/// This is the single rule both sources end at, and it deliberately **does not transform** the value:
/// nothing is trimmed, normalised or re-encoded. A `file:` written by `echo` therefore carries its
/// newline into the password, exactly as `env:` carries whatever the variable holds. A transform here
/// would be invisible in this process and fatal against every other etcd client, because it would
/// make this one the only caller that presents a different secret than the one configured.
///
/// The size bound covers the `env:` source, whose value no file read bounds; a `file:` value that
/// survived its read is within it already.
fn accepted(value: String) -> Result<String, SourceBuildError> {
    if value.is_empty() {
        return Err(setting(PASSWORD_REF, "the password is empty"));
    }
    if value.len() > MAX_PASSWORD_BYTES {
        return Err(setting(
            PASSWORD_REF,
            "the password is larger than the accepted bound",
        ));
    }
    Ok(value)
}

/// Reads one file a `tls` block names.
///
/// The ceiling is the bootstrap's own [`MAX_TLS_MATERIAL_BYTES`]: the second read applies the same
/// bound the first one did, so a file that passed the bootstrap cannot fail here on size — only on
/// content, which is the swap the second read exists to catch.
fn read_file(path: &Path, field: &'static str) -> Result<Vec<u8>, SourceBuildError> {
    nico_config::read_bounded(path, MAX_TLS_MATERIAL_BYTES).map_err(|error| match error {
        nico_config::ReadError::TooLarge { .. } => {
            setting(field, "is larger than the accepted bound")
        }
        nico_config::ReadError::Io(source) => {
            setting(field, format!("could not be read: {source}"))
        }
        _ => setting(field, format!("could not be read: {error}")),
    })
}

/// Builds the error for a bootstrap field that cannot be honoured while the source is built.
fn setting(field: &'static str, reason: impl Into<Cow<'static, str>>) -> SourceBuildError {
    SourceBuildError::Setting {
        field: field.into(),
        reason: reason.into(),
    }
}

/// The dotted path of the trust root.
const CA_FILE: &str = "config_source.tls.ca_file";

/// The dotted path of the client certificate.
const CERT_FILE: &str = "config_source.tls.cert_file";

/// The dotted path of the client private key.
const KEY_FILE: &str = "config_source.tls.key_file";

/// The dotted path of the password reference.
const PASSWORD_REF: &str = "config_source.auth.password_ref";

#[cfg(test)]
mod tests;
