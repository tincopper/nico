//! Turning a validated `quota_store` selection into the one store the process serves with.
//!
//! The assembly is the only place that names a concrete backend: the bootstrap states *what* the
//! deployment selected, this module decides *what this build can construct*, and everything
//! downstream — the publication handle, the request hooks, the reload — holds an
//! `Arc<dyn BudgetStore>` and never learns which backend sits behind it. A selection this build
//! cannot serve (a backend whose feature is off, credentials that cannot be read) is a startup
//! refusal, exactly like a source this build cannot serve: serving budgets from the wrong store
//! is worse than not starting.

use std::sync::Arc;

use nico_http::BudgetStore;

use crate::bootstrap::{BootstrapConfig, QuotaStoreSelection, UnavailablePolicy};

/// Why a quota store could not be built from a validated selection.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum QuotaStoreBuildError {
    /// The bootstrap selected a backend this build was not compiled with.
    #[error("quota_store type `{found}` is not compiled into this build")]
    Unsupported {
        /// The rejected backend type.
        found: &'static str,
    },
    /// A reference the selection carries (a password `env:`/`file:` reference) could not be read.
    #[error("quota_store field `{field}` is invalid: {reason}")]
    Setting {
        /// Dotted field path, for example `quota_store.etcd.auth.password_ref`.
        field: std::borrow::Cow<'static, str>,
        /// Why the value was rejected. Never contains the secret itself.
        reason: std::borrow::Cow<'static, str>,
    },
    /// The backend client could not be constructed.
    #[error("the quota store backend could not be built: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
}

// Compiled-in proof that a build error can cross `.await` and `tokio::spawn` boundaries.
const _: fn() = || {
    const fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<QuotaStoreBuildError>();
};

/// Reads one `env:`/`file:` password reference under the config source's own conventions:
/// `env:` as written, `file:` byte for byte — a trailing newline written by `echo` is part of
/// the password, and nothing is trimmed. The value never reaches an error message.
pub(crate) fn read_password_ref(reference: &str) -> Result<String, std::borrow::Cow<'static, str>> {
    /// The largest accepted password, matching the config source's bound.
    const MAX_PASSWORD_BYTES: usize = 1024;

    if let Some(name) = reference.strip_prefix("env:") {
        if name.is_empty() {
            return Err(std::borrow::Cow::Borrowed("the reference names nothing"));
        }
        return std::env::var(name).map_err(|error| match error {
            // `VarError`'s own message for a non-UTF-8 value quotes the value, which is the one
            // thing that must never reach a log; a fixed sentence replaces it.
            std::env::VarError::NotUnicode(_) => {
                std::borrow::Cow::Borrowed("the environment variable is not valid UTF-8")
            }
            std::env::VarError::NotPresent => {
                std::borrow::Cow::Borrowed("the environment variable is not set")
            }
        });
    }
    let Some(path) = reference.strip_prefix("file:") else {
        return Err(std::borrow::Cow::Borrowed(
            "the reference must begin with `env:` or `file:`",
        ));
    };
    if path.is_empty() {
        return Err(std::borrow::Cow::Borrowed("the reference names nothing"));
    }
    let bytes = nico_config::read_bounded(std::path::Path::new(path), MAX_PASSWORD_BYTES).map_err(
        |error| match error {
            nico_config::ReadError::TooLarge { .. } => {
                std::borrow::Cow::Borrowed("the password is larger than the accepted bound")
            }
            _ => std::borrow::Cow::Borrowed("the password cannot be read"),
        },
    )?;
    let value = String::from_utf8(bytes)
        .map_err(|_| std::borrow::Cow::Borrowed("the password is not valid UTF-8"))?;
    if value.is_empty() {
        return Err(std::borrow::Cow::Borrowed("the password is empty"));
    }
    Ok(value)
}

/// Builds the budget store the bootstrap selects, and the failure policy it is served with.
///
/// The policy travels beside the store because it is a *deployment* choice about the backend's
/// absence, not a property of the store itself: the same backend can be run fail-open by one
/// deployment and fail-closed by another, and the request hook must be able to look the answer
/// up without knowing which backend answered.
///
/// # Errors
/// Returns [`QuotaStoreBuildError`] when the selection names a backend this build cannot serve, a
/// credential reference cannot be read, or the backend client fails to construct.
#[cfg_attr(not(feature = "etcd"), allow(clippy::unused_async))]
pub async fn build_quota_store(
    bootstrap: &BootstrapConfig,
) -> Result<(Arc<dyn BudgetStore>, UnavailablePolicy), QuotaStoreBuildError> {
    match bootstrap.quota_store() {
        QuotaStoreSelection::Local => Ok((
            Arc::new(nico_http::InProcessBudgetStore::new(10_000)),
            UnavailablePolicy::FailClosed,
        )),
        #[cfg(feature = "etcd")]
        QuotaStoreSelection::Etcd {
            endpoints,
            key_prefix,
            tls,
            auth,
            ..
        } => {
            let options = quota_etcd::connect_options(tls.as_ref(), auth.as_ref())?;
            let store = nico_quota_storage::etcd::EtcdBudgetStore::connect(
                endpoints,
                key_prefix,
                Some(options),
            )
            .await
            .map_err(|error| QuotaStoreBuildError::Backend(Box::new(error)))?;
            Ok((Arc::new(store), bootstrap.quota_policy()))
        }
        #[cfg(not(feature = "etcd"))]
        QuotaStoreSelection::Etcd { .. } => {
            Err(QuotaStoreBuildError::Unsupported { found: "etcd" })
        }
        #[cfg(feature = "redis")]
        QuotaStoreSelection::Redis {
            endpoints,
            key_prefix,
            ..
        } => {
            let store = nico_quota_storage::redis::RedisBudgetStore::connect(endpoints, key_prefix)
                .await
                .map_err(|error| QuotaStoreBuildError::Backend(Box::new(error)))?;
            Ok((Arc::new(store), bootstrap.quota_policy()))
        }
        #[cfg(not(feature = "redis"))]
        QuotaStoreSelection::Redis { .. } => {
            Err(QuotaStoreBuildError::Unsupported { found: "redis" })
        }
    }
}

/// The etcd wire assembly the quota store uses: the same material rules the config source's
/// branch follows, under `quota_store.` field paths.
#[cfg(feature = "etcd")]
mod quota_etcd {
    use crate::{
        bootstrap::{QuotaEtcdAuth, QuotaEtcdTls},
        quota_build::{QuotaStoreBuildError, read_password_ref},
    };

    /// Builds the wire connect options from the validated blocks.
    ///
    /// # Errors
    /// Returns [`QuotaStoreBuildError::Setting`] when a file the TLS block names cannot be read
    /// or is no longer usable, or when the password reference cannot be resolved.
    pub(super) fn connect_options(
        tls: Option<&QuotaEtcdTls>,
        auth: Option<&QuotaEtcdAuth>,
    ) -> Result<etcd_client::ConnectOptions, QuotaStoreBuildError> {
        let mut wire = etcd_client::ConnectOptions::new();
        if let Some(tls) = tls {
            let ca = read_file(&tls.ca_file, "quota_store.etcd.tls.ca_file")?;
            nico_http::validate_bundle(&ca).map_err(|error| QuotaStoreBuildError::Setting {
                field: "quota_store.etcd.tls.ca_file".into(),
                reason: format!("is not a usable trust bundle: {error}").into(),
            })?;
            let mut config = etcd_client::TlsOptions::new()
                .ca_certificate(etcd_client::Certificate::from_pem(ca));
            if let Some((cert_file, key_file)) = &tls.identity {
                let certificate = read_file(cert_file, "quota_store.etcd.tls.cert_file")?;
                let key = read_file(key_file, "quota_store.etcd.tls.key_file")?;
                config = config.identity(etcd_client::Identity::from_pem(certificate, key));
            }
            if let Some(domain_name) = &tls.domain_name {
                config = config.domain_name(domain_name.clone());
            }
            wire = wire.with_tls(config);
        }
        if let Some(auth) = auth {
            let password = read_password_ref(&auth.password_ref).map_err(|reason| {
                QuotaStoreBuildError::Setting {
                    field: "quota_store.etcd.auth.password_ref".into(),
                    reason,
                }
            })?;
            wire = wire.with_user(auth.user.clone(), password);
        }
        Ok(wire)
    }

    /// Reads one TLS file, naming the field rather than the bytes.
    fn read_file(
        path: &std::path::Path,
        field: &'static str,
    ) -> Result<Vec<u8>, QuotaStoreBuildError> {
        std::fs::read(path).map_err(|error| QuotaStoreBuildError::Setting {
            field: field.into(),
            reason: format!("could not be read: {error}").into(),
        })
    }
}

#[cfg(test)]
mod tests;
