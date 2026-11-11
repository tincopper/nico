//! The one place that maps [`TlsPolicy`] onto Pingora's TLS configuration.
//!
//! This is the only file in the crate that imports a Pingora TLS type, and it exists so that
//! `policy.rs` can stay framework-free: if a second TLS consumer ever arrives (a remote
//! configuration source, an exporter, plugin egress), the policy moves and only this file is
//! rewritten.
//!
//! What it writes is deliberately one field. `verify_cert` and `verify_hostname` are always true,
//! and a peer's defaults already say so — mapping "set the default" would be an abstraction with no
//! second consumer. When a peer-level fact becomes real (a per-provider CA, a client certificate),
//! this module grows an `apply_peer` step beside `apply`.

use pingora::server::configuration::ServerConf;

use super::{RootSource, TlsPolicy};

/// Writes the policy into the process-level configuration Pingora builds its connector from.
///
/// Pingora reads this value **once**, when the service is built
/// (`ProxyServiceBuilder::build` → `ConnectorOptions::from_server_conf`), which is why the policy is
/// a startup fact: a reload changes the document snapshot, never the connector.
///
/// The path is written as text because that is the field's type; Pingora opens it while building
/// the connector, so the file must also be readable by the process at that moment. It was already
/// read and validated at bootstrap, which is what keeps a bad bundle from reaching that `unwrap()`.
///
/// The conversion is lossless for the caller this crate has: a `PathBuf` that came out of the
/// bootstrap document was a UTF-8 string, and [`Path::display`] only substitutes characters for
/// bytes that are not. A future caller that builds a policy from an OS string must reject a path it
/// cannot express as text, or the connector would open a different file than [`ensure_usable`]
/// checked.
///
/// [`Path::display`]: std::path::Path::display
/// [`ensure_usable`]: super::ensure_usable
pub(crate) fn apply(policy: &TlsPolicy, conf: &mut ServerConf) {
    match policy.root_source() {
        // Nothing to write: Pingora's own default is the platform store, and every `SSL_CERT_FILE` /
        // `SSL_CERT_DIR` it honours is read there.
        RootSource::Platform => {}
        RootSource::Bundle(path) => {
            conf.ca_file = Some(path.display().to_string());
        }
    }
}
