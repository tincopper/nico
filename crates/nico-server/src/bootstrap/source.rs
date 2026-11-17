//! The `config_source` section: which authoritative source this process reads its document from.
//!
//! The section is per-type by design: a local document has a path and file-watch triggers, an etcd
//! key has endpoints and a key name, and neither can describe the other — writing `path` into an
//! etcd source declaration is an unknown-field startup error, and so is the reverse. The validated
//! selection is the [`SourceSelection`]; it carries plain data so the construction site decides
//! which type it can build.
//!
//! Two rules govern the type dispatch:
//!
//! - **A validated bootstrap never represents a source this build cannot serve.** `etcd` is
//!   accepted only when compiled with the `etcd` feature; every other build refuses it at load
//!   with the list of types it does serve.
//! - **The endpoints pass through untouched.** Multi-endpoint failover is the etcd client's own
//!   behaviour beneath the [`crate::source seam`](nico_config::ConfigSource); the bootstrap bounds
//!   only the *shape* of each endpoint so a typo fails at startup with a field name.

use std::path::PathBuf;

use nico_config::WatchMode;
use serde::Deserialize;
use serde_norway::Value;

use super::BootstrapError;

/// The one authoritative source a bootstrap file selects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceSelection {
    /// A local configuration document, watched by file notifications and/or a signal.
    Local {
        /// The document path.
        path: PathBuf,
        /// How the local source detects changes.
        watch_mode: WatchMode,
    },
    /// One etcd key holding the complete configuration document, reached through any endpoint.
    #[cfg(feature = "etcd")]
    Etcd {
        /// The endpoints the client balances across.
        endpoints: Vec<String>,
        /// The key holding the document.
        key: String,
    },
}

/// The `config_source` body's types, for the unsupported-type error text.
fn supported_sources() -> &'static str {
    if cfg!(feature = "etcd") {
        "`local`, `etcd`"
    } else {
        "`local`"
    }
}

/// Reads the `config_source` body into its typed selection and its shared size bound.
///
/// The body is parsed in two steps because a tagged enum cannot also deny unknown fields: the
/// `type` value is read first, and only the matching schema is applied to the full body — so a
/// wrong field for the declared type is a startup error, never an ignored one.
///
/// # Errors
/// - [`BootstrapError::Parse`] when the body does not match the schema of its own declared type.
/// - [`BootstrapError::UnsupportedSource`] when the type is not one this build serves.
/// - [`BootstrapError::InvalidField`] when the etcd endpoints or key are not usable shapes.
pub(super) fn parse_source(value: Value) -> Result<(SourceSelection, usize), BootstrapError> {
    let kind = serde_norway::from_value::<RawSourceKind>(value.clone())
        .map_err(|source| BootstrapError::Parse { source })?
        .kind;
    match kind.as_str() {
        "local" => {
            let raw: RawLocalSource = serde_norway::from_value(value)
                .map_err(|source| BootstrapError::Parse { source })?;
            let watch_mode = parse_watch_mode(&raw.watch)?;
            Ok((
                SourceSelection::Local {
                    path: raw.path,
                    watch_mode,
                },
                raw.max_bytes,
            ))
        }
        #[cfg(feature = "etcd")]
        "etcd" => {
            let raw: RawEtcdSource = serde_norway::from_value(value)
                .map_err(|source| BootstrapError::Parse { source })?;
            validate_endpoints(&raw.endpoints)?;
            validate_key(&raw.key)?;
            Ok((
                SourceSelection::Etcd {
                    endpoints: raw.endpoints,
                    key: raw.key,
                },
                raw.max_bytes,
            ))
        }
        other => Err(BootstrapError::UnsupportedSource {
            found: other.to_owned(),
            serves: supported_sources(),
        }),
    }
}

/// The discriminator every `config_source` body carries, read before the type is known.
#[derive(Debug, Deserialize)]
struct RawSourceKind {
    #[serde(rename = "type")]
    kind: String,
}

/// The `local` body exactly as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLocalSource {
    #[serde(rename = "type")]
    _kind: String,
    path: PathBuf,
    max_bytes: usize,
    watch: String,
}

/// The `etcd` body exactly as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg(feature = "etcd")]
struct RawEtcdSource {
    #[serde(rename = "type")]
    _kind: String,
    endpoints: Vec<String>,
    key: String,
    max_bytes: usize,
}

/// Parses the local source's trigger modes, accepting the two triggers and their combination.
fn parse_watch_mode(value: &str) -> Result<WatchMode, BootstrapError> {
    match value {
        "notify" => Ok(WatchMode::Notify),
        "signal" => Ok(WatchMode::Signal),
        "notify+signal" | "signal+notify" => Ok(WatchMode::NotifyAndSignal),
        _ => Err(BootstrapError::InvalidField {
            field: "config_source.watch".into(),
            reason: "must be one of `notify`, `signal`, `notify+signal`".into(),
        }),
    }
}

/// Validates the endpoint list: at least one endpoint, each a usable shape.
#[cfg(feature = "etcd")]
fn validate_endpoints(endpoints: &[String]) -> Result<(), BootstrapError> {
    if endpoints.is_empty() {
        return Err(BootstrapError::InvalidField {
            field: "config_source.endpoints".into(),
            reason: "must list at least one endpoint".into(),
        });
    }
    for (index, endpoint) in endpoints.iter().enumerate() {
        validate_endpoint(endpoint, index)?;
    }
    Ok(())
}

/// Validates one endpoint's shape.
///
/// `etcd-client` accepts an `http://` endpoint as written, prefixes `http://` onto a bare
/// `host:port`, and this build links no TLS feature — an `https` endpoint can never be served, so
/// it is refused here with that reason instead of at connect. The full URI parse belongs to the
/// client; a shape-bounded endpoint that survives this check fails at connect only when the host
/// is genuinely wrong.
#[cfg(feature = "etcd")]
fn validate_endpoint(endpoint: &str, index: usize) -> Result<(), BootstrapError> {
    let field = format!("config_source.endpoints[{index}]");
    let invalid = |reason: String| BootstrapError::InvalidField {
        field: field.clone().into(),
        reason: reason.into(),
    };

    if endpoint.trim() != endpoint {
        return Err(invalid("must not contain surrounding whitespace".into()));
    }
    let authority = if let Some(rest) = endpoint.strip_prefix("http://") {
        rest
    } else if endpoint.starts_with("https://") {
        return Err(invalid(
            "is an https endpoint, but this build's etcd client has no TLS feature; serve an \
             http:// endpoint"
                .into(),
        ));
    } else {
        endpoint
    };
    // Everything after the first `/` is a path the etcd client would only serve the gRPC protocol
    // on; a declared path cannot be honoured, so the authority alone is what is validated.
    let authority = authority.split('/').next().unwrap_or("");
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };

    if host.is_empty() || (host.contains(':') && !host.starts_with('[')) {
        return Err(invalid(format!("has an unusable host `{host}`")));
    }
    if let Some(port) = port {
        let port: u16 = port
            .parse()
            .map_err(|_| invalid(format!("has an unusable port `{port}`; it must be numeric")))?;
        if port == 0 {
            return Err(invalid("has port 0; ports start at 1".into()));
        }
    }
    Ok(())
}

/// Validates the watched key: a non-empty, unpadded key name.
#[cfg(feature = "etcd")]
fn validate_key(key: &str) -> Result<(), BootstrapError> {
    if key.trim() != key || key.is_empty() {
        return Err(BootstrapError::InvalidField {
            field: "config_source.key".into(),
            reason: "must be a non-empty key name without surrounding whitespace".into(),
        });
    }
    Ok(())
}
