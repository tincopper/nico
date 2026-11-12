//! One `upstreams` entry of a fixture document.
//!
//! The entry is its own file because it owns the facts a scenario varies: the scheme, the logical host
//! a certificate is verified against, and — once an upstream has more than one member — the member
//! list and the failure budget its members are judged by. A host that is an *address* rather than a
//! name is the mechanism behind the hostname-mismatch scenario — the peer resolves, the chain verifies
//! and the name does not match — so the mapping from a socket address to `(host, port)` is part of
//! this structure's job rather than a detail of the renderer that happens to hold it.
//!
//! The single-address form writes no `endpoints` at all, which is the document form every other
//! scenario keeps using: emitting an explicit one-member list would make those fixtures depend on a
//! schema feature they do not exercise.

use std::net::SocketAddr;

/// One entry of the document's `upstreams` collection.
#[derive(Debug, Clone)]
pub(super) struct UpstreamEntry {
    name: String,
    scheme: &'static str,
    host: String,
    port: u16,
    /// The member list, empty for the single-address form.
    members: Vec<MemberEntry>,
    /// The selection rule, when a scenario needs one other than the document default.
    strategy: Option<&'static str>,
    /// `(max_fails, fail_timeout_ms)`, when a scenario tunes the failure budget.
    ejection: Option<(u32, u32)>,
}

/// One `endpoints[]` entry: a connection target with its own name.
#[derive(Debug, Clone)]
struct MemberEntry {
    name: &'static str,
    address: SocketAddr,
}

impl UpstreamEntry {
    /// A plaintext entry addressed by `host`:`port`.
    pub(super) fn plaintext(name: &str, host: String, port: u16) -> Self {
        Self::single(name, "http", host, port)
    }

    /// An `https` entry addressed by the name its certificate is signed for.
    pub(super) fn tls(name: &str, host: &str, port: u16) -> Self {
        Self::single(name, "https", host.to_owned(), port)
    }

    /// An entry with an explicit member list, addressed by `host`.
    ///
    /// `host` stays the logical identity — `Host` and the TLS server name — while every member
    /// connects to its own address, which is what makes one certificate serve a fleet.
    pub(super) fn members(
        name: &str,
        scheme: &'static str,
        host: &str,
        members: &[(&'static str, SocketAddr)],
    ) -> Self {
        Self {
            name: name.to_owned(),
            scheme,
            host: host.to_owned(),
            port: members.first().map_or(80, |(_, address)| address.port()),
            members: members
                .iter()
                .map(|(member, address)| MemberEntry {
                    name: member,
                    address: *address,
                })
                .collect(),
            strategy: None,
            ejection: None,
        }
    }

    /// The single-address form: one member, named by the document's default.
    fn single(name: &str, scheme: &'static str, host: String, port: u16) -> Self {
        Self {
            name: name.to_owned(),
            scheme,
            host,
            port,
            members: Vec::new(),
            strategy: None,
            ejection: None,
        }
    }

    /// Sets the upstream's `strategy`.
    pub(super) fn with_strategy(mut self, strategy: &'static str) -> Self {
        self.strategy = Some(strategy);
        self
    }

    /// Sets the upstream's `max_fails` and `fail_timeout_ms`.
    pub(super) fn with_ejection(mut self, max_fails: u32, fail_timeout_ms: u32) -> Self {
        self.ejection = Some((max_fails, fail_timeout_ms));
        self
    }

    /// Renders the entry as the lines it occupies under `upstreams:`.
    pub(super) fn render(&self) -> String {
        let mut entry = format!(
            "  - name: {name}\n    scheme: {scheme}\n    host: {host}\n    port: {port}\n",
            name = self.name,
            scheme = self.scheme,
            host = self.host,
            port = self.port,
        );
        if let Some(strategy) = self.strategy {
            entry.push_str(&format!("    strategy: {strategy}\n"));
        }
        if let Some((max_fails, fail_timeout_ms)) = self.ejection {
            entry.push_str(&format!("    max_fails: {max_fails}\n"));
            entry.push_str(&format!("    fail_timeout_ms: {fail_timeout_ms}\n"));
        }
        if !self.members.is_empty() {
            entry.push_str("    endpoints:\n");
            for member in &self.members {
                entry.push_str(&format!(
                    "      - name: {name}\n        address: {address}\n        port: {port}\n",
                    name = member.name,
                    address = member.address.ip(),
                    port = member.address.port(),
                ));
            }
        }
        entry
    }
}
