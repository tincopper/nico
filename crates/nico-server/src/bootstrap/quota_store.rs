//! The `quota_store` section: where token budgets live when one process is not enough.
//!
//! Omitted, the section means the standing in-process table — the single-replica default, and
//! the only behaviour every earlier bootstrap file ever had. Written, it selects a shared backend
//! (`etcd` or `redis`) and the failure policy that governs what a request is answered when that
//! backend cannot confirm an admission.
//!
//! The section is per-type by design, the same way `config_source` is: an etcd body has
//! endpoints, TLS and auth; a Redis body has its own endpoint form; neither can describe the
//! other, and a field written for the wrong type is an unknown-field startup error rather than an
//! ignored one.
//!
//! Two rules govern the section:
//!
//! - **A shared store must state its failure policy.** `on_unavailable` is required for every
//!   backend this section can name: a deployment that cannot be answered by its quota storage has
//!   to choose between refusing the request (`fail-closed`) and letting it through (`fail-open`),
//!   and neither choice is one the gateway makes on the operator's behalf.
//! - **The section is never served by a configuration source.** Like every other bootstrap
//!   section, it is read before any source exists: the endpoints, credentials and namespace of the
//!   quota store are process facts, not document facts.

use std::{borrow::Cow, path::PathBuf};

/// The failure policy a shared quota store is configured with.
///
/// The domain type, re-exported: the bootstrap parses the written value into the same enum the
/// request hook reads, so the two ends of the process can never disagree on what the deployment
/// stated.
pub use nico_http::UnavailablePolicy;
use serde::Deserialize;
use serde_norway::Value;

use super::BootstrapError;

/// Parses the written policy value into the domain's enum.
///
/// # Errors
/// Returns [`BootstrapError::InvalidField`] for anything but the two documented values.
pub(super) fn parse_unavailable_policy(
    field: &str,
    value: &str,
) -> Result<UnavailablePolicy, BootstrapError> {
    match value {
        "fail-closed" => Ok(UnavailablePolicy::FailClosed),
        "fail-open" => Ok(UnavailablePolicy::FailOpen),
        other => Err(BootstrapError::InvalidField {
            field: Cow::Owned(field.to_owned()),
            reason: format!("must be `fail-closed` or `fail-open` (found `{other}`)").into(),
        }),
    }
}

/// The transport one quota-store etcd connection uses: paths only, never bytes — the same shape
/// the config source's block keeps, validated against `quota_store.` field paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaEtcdTls {
    /// The trust root every endpoint is verified against.
    pub ca_file: PathBuf,
    /// The client identity as a `(certificate chain, private key)` pair, when configured.
    pub identity: Option<(PathBuf, PathBuf)>,
    /// The name the presented certificate must carry, when written.
    pub domain_name: Option<String>,
}

/// The credentials one quota-store etcd connection authenticates with: a user and a password
/// *reference*, never a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaEtcdAuth {
    /// The user presented to etcd.
    pub user: String,
    /// The `env:`/`file:` reference the password is read from at assembly.
    pub password_ref: String,
}

/// The validated `quota_store` section.
///
/// Plain data, like [`SourceSelection`](super::SourceSelection): the construction site decides
/// which backend this build can assemble, and the request path never learns more than the
/// store it is handed.
#[derive(Debug, Clone, PartialEq)]
pub enum QuotaStoreSelection {
    /// No section: the standing in-process table. No failure policy is needed, because there is
    /// no storage that can be unavailable.
    Local,
    /// One etcd cluster holds the windows, one transaction per admission or charge.
    Etcd {
        /// The endpoints the client balances across.
        endpoints: Vec<String>,
        /// The prefix every budget key is written under.
        key_prefix: String,
        /// The transport every endpoint is reached over, when the section configures TLS.
        tls: Option<QuotaEtcdTls>,
        /// The credentials the client authenticates with, when the section configures them.
        auth: Option<QuotaEtcdAuth>,
        /// What an unconfirmable admission is answered with.
        on_unavailable: UnavailablePolicy,
    },
    /// One Redis deployment holds the windows, one script/transaction per admission or charge.
    Redis {
        /// The endpoints the client connects to.
        endpoints: Vec<String>,
        /// The prefix every budget key is written under.
        key_prefix: String,
        /// What an unconfirmable admission is answered with.
        on_unavailable: UnavailablePolicy,
    },
}

/// Largest accepted `quota_store.key_prefix`, in bytes.
const MAX_KEY_PREFIX_BYTES: usize = 256;

/// Largest accepted endpoint, in bytes.
const MAX_ENDPOINT_BYTES: usize = 2048;

/// The `etcd` body exactly as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEtcdQuota {
    endpoints: Vec<String>,
    #[serde(default)]
    tls: Option<RawQuotaTls>,
    #[serde(default)]
    auth: Option<RawQuotaAuth>,
}

/// The `quota_store.etcd.tls` block exactly as written.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawQuotaTls {
    ca_file: Option<PathBuf>,
    cert_file: Option<PathBuf>,
    key_file: Option<PathBuf>,
    domain_name: Option<String>,
}

/// The `quota_store.etcd.auth` block exactly as written.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawQuotaAuth {
    user: Option<String>,
    password_ref: Option<String>,
}

/// Reads the `quota_store` body into its typed selection.
///
/// The body is parsed in the same two steps `config_source` uses: the `type` value is read first,
/// and only the matching schema is applied to the full body — so a wrong field for the declared
/// type is a startup error, never an ignored one.
///
/// # Errors
/// - [`BootstrapError::Parse`] when the body does not match the schema of its own declared type.
/// - [`BootstrapError::InvalidField`] when a namespace, endpoint, TLS or auth value is not usable.
pub(super) fn parse_quota_store(value: Value) -> Result<QuotaStoreSelection, BootstrapError> {
    #[derive(Deserialize)]
    struct RawKind {
        #[serde(rename = "type")]
        kind: String,
    }
    let kind = serde_norway::from_value::<RawKind>(value.clone())
        .map_err(|source| BootstrapError::Parse { source })?
        .kind;

    // The discriminator names the schema; it is not one of the backend's fields. Take it out
    // the way `config_source` does, so a backend body's closed shape never sees it.
    let mut body = value;
    if let Value::Mapping(mapping) = &mut body {
        mapping.remove("type");
    }

    match kind.as_str() {
        "etcd" => parse_etcd(body),
        "redis" => parse_redis(body),
        other => Err(BootstrapError::InvalidField {
            field: "quota_store.type".into(),
            reason: format!("must be `etcd` or `redis` (found `{other}`)").into(),
        }),
    }
}

/// The shared fields every backend body carries, taken out of the body before the
/// backend-specific schema sees it.
struct SharedFields {
    key_prefix: String,
    on_unavailable: UnavailablePolicy,
}

/// Reads and removes the shared keys from a section body.
fn take_shared(body: &mut Value) -> Result<SharedFields, BootstrapError> {
    #[derive(Deserialize)]
    struct RawShared {
        key_prefix: String,
        on_unavailable: String,
    }
    let Value::Mapping(body) = body else {
        return Err(BootstrapError::Parse {
            source: serde::de::Error::custom("the quota_store body must be a mapping"),
        });
    };
    let raw: RawShared = serde_norway::from_value(Value::Mapping(body.clone()))
        .map_err(|source| BootstrapError::Parse { source })?;
    body.remove("key_prefix");
    body.remove("on_unavailable");

    validate_label(
        "quota_store.key_prefix",
        &raw.key_prefix,
        MAX_KEY_PREFIX_BYTES,
    )?;
    Ok(SharedFields {
        key_prefix: raw.key_prefix,
        on_unavailable: parse_unavailable_policy(
            "quota_store.on_unavailable",
            &raw.on_unavailable,
        )?,
    })
}

/// Validates a bounded printable-ASCII label: non-empty, within the byte bound, no control
/// characters and no spaces the storage layer could not round-trip.
fn validate_label(field: &str, value: &str, max_bytes: usize) -> Result<(), BootstrapError> {
    let field = Cow::Owned(field.to_owned());
    if value.is_empty() {
        return Err(BootstrapError::InvalidField {
            field,
            reason: "must not be empty".into(),
        });
    }
    if value.len() > max_bytes {
        return Err(BootstrapError::InvalidField {
            field,
            reason: format!("must be at most {max_bytes} bytes (found {})", value.len()).into(),
        });
    }
    if !value.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) {
        return Err(BootstrapError::InvalidField {
            field,
            reason: "must be printable ASCII without spaces".into(),
        });
    }
    Ok(())
}

/// Validates that an endpoint list is present and every entry is usable.
fn validate_endpoints(field: &str, endpoints: &[String]) -> Result<(), BootstrapError> {
    let field = Cow::Owned(field.to_owned());
    if endpoints.is_empty() {
        return Err(BootstrapError::InvalidField {
            field,
            reason: "must list at least one endpoint".into(),
        });
    }
    for endpoint in endpoints {
        validate_label(&field, endpoint, MAX_ENDPOINT_BYTES)?;
    }
    Ok(())
}

/// Reads the etcd body: the shared half plus the etcd transport and credentials.
fn parse_etcd(value: Value) -> Result<QuotaStoreSelection, BootstrapError> {
    #[cfg(feature = "etcd")]
    {
        let mut body = value;
        let shared = take_shared(&mut body)?;
        let raw: RawEtcdQuota =
            serde_norway::from_value(body).map_err(|source| BootstrapError::Parse { source })?;
        validate_endpoints("quota_store.etcd.endpoints", &raw.endpoints)?;
        let tls = raw.tls.as_ref().map(resolve_tls).transpose()?;
        let auth = raw.auth.as_ref().map(resolve_auth).transpose()?;
        Ok(QuotaStoreSelection::Etcd {
            endpoints: raw.endpoints,
            key_prefix: shared.key_prefix,
            tls,
            auth,
            on_unavailable: shared.on_unavailable,
        })
    }
    #[cfg(not(feature = "etcd"))]
    {
        let _ = value;
        Err(BootstrapError::InvalidField {
            field: "quota_store.type".into(),
            reason: "this build was compiled without the etcd feature".into(),
        })
    }
}

/// Reads the Redis body: the shared half plus the endpoint list this adapter requires.
fn parse_redis(value: Value) -> Result<QuotaStoreSelection, BootstrapError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RawRedis {
        endpoints: Vec<String>,
    }
    let mut body = value;
    let shared = take_shared(&mut body)?;
    let raw: RawRedis =
        serde_norway::from_value(body).map_err(|source| BootstrapError::Parse { source })?;
    validate_endpoints("quota_store.redis.endpoints", &raw.endpoints)?;
    Ok(QuotaStoreSelection::Redis {
        endpoints: raw.endpoints,
        key_prefix: shared.key_prefix,
        on_unavailable: shared.on_unavailable,
    })
}

/// Resolves the quota-store etcd TLS block, validating the same PEM facts the config source's
/// block does but reporting under `quota_store.etcd.tls.*`.
///
/// # Errors
/// Returns [`BootstrapError::InvalidField`] for a missing trust root, a half-written identity, an
/// unusable bundle or a domain name no certificate can carry.
#[cfg(feature = "etcd")]
fn resolve_tls(raw: &RawQuotaTls) -> Result<QuotaEtcdTls, BootstrapError> {
    use nico_http::{validate_bundle, validate_server_identity};

    const TLS: &str = "quota_store.etcd.tls";
    let Some(ca_file) = raw
        .ca_file
        .as_ref()
        .filter(|path| !path.as_os_str().is_empty())
    else {
        return Err(BootstrapError::InvalidField {
            field: format!("{TLS}.ca_file").into(),
            reason: "is required when the `tls` block is present; the trust root is never taken \
                     from the platform store"
                .into(),
        });
    };
    let bytes = std::fs::read(ca_file).map_err(|source| BootstrapError::InvalidField {
        field: format!("{TLS}.ca_file").into(),
        reason: format!("could not be read: {source}").into(),
    })?;
    validate_bundle(&bytes).map_err(|error| BootstrapError::InvalidField {
        field: format!("{TLS}.ca_file").into(),
        reason: format!("is not a usable trust bundle: {error}").into(),
    })?;

    let identity = match (raw.cert_file.as_ref(), raw.key_file.as_ref()) {
        (None, None) => None,
        (Some(cert_file), Some(key_file)) => {
            let certificate =
                std::fs::read(cert_file).map_err(|source| BootstrapError::InvalidField {
                    field: format!("{TLS}.cert_file").into(),
                    reason: format!("could not be read: {source}").into(),
                })?;
            let key = std::fs::read(key_file).map_err(|source| BootstrapError::InvalidField {
                field: format!("{TLS}.key_file").into(),
                reason: format!("could not be read: {source}").into(),
            })?;
            validate_server_identity(&certificate, &key).map_err(|error| {
                BootstrapError::InvalidField {
                    field: format!("{TLS}.{}", error.field()).into(),
                    reason: format!("is not a usable TLS identity: {error}").into(),
                }
            })?;
            Some((cert_file.clone(), key_file.clone()))
        }
        _ => {
            return Err(BootstrapError::InvalidField {
                field: format!("{TLS}.key_file").into(),
                reason: "must be written together with `cert_file`".into(),
            });
        }
    };

    let domain_name = match raw.domain_name.as_deref() {
        None => None,
        Some(name) => {
            nico_core::validate_host(name).map_err(|error| BootstrapError::InvalidField {
                field: format!("{TLS}.domain_name").into(),
                reason: format!("is not a usable certificate name: {error:?}").into(),
            })?;
            Some(name.to_owned())
        }
    };

    Ok(QuotaEtcdTls {
        ca_file: ca_file.clone(),
        identity,
        domain_name,
    })
}

/// Resolves the quota-store etcd auth block: a user and a password *reference*, reported under
/// `quota_store.etcd.auth.*`.
///
/// # Errors
/// Returns [`BootstrapError::InvalidField`] when either half is missing or a password is written
/// as a value.
#[cfg(feature = "etcd")]
fn resolve_auth(raw: &RawQuotaAuth) -> Result<QuotaEtcdAuth, BootstrapError> {
    const AUTH: &str = "quota_store.etcd.auth";
    let Some(user) = raw.user.as_deref().filter(|user| !user.is_empty()) else {
        return Err(BootstrapError::InvalidField {
            field: format!("{AUTH}.user").into(),
            reason: "is required when the `auth` block is present".into(),
        });
    };
    let Some(password_ref) = raw.password_ref.as_deref().filter(|r| !r.is_empty()) else {
        return Err(BootstrapError::InvalidField {
            field: format!("{AUTH}.password_ref").into(),
            reason: "is required when the `auth` block is present".into(),
        });
    };
    if !password_ref.starts_with("env:") && !password_ref.starts_with("file:") {
        return Err(BootstrapError::InvalidField {
            field: format!("{AUTH}.password_ref").into(),
            reason: "must be an `env:` or `file:` reference; a password is never a value".into(),
        });
    }
    Ok(QuotaEtcdAuth {
        user: user.to_owned(),
        password_ref: password_ref.to_owned(),
    })
}
