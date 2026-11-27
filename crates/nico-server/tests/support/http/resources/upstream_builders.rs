//! The upstream-related builder steps of [`Resources`](super::Resources), extracted when the
//! health-check step joined: the file holds the document envelope and the render, this one holds
//! the steps that add and tune upstreams.

use std::net::SocketAddr;

use super::{Resources, UpstreamEntry};

impl Resources {
    /// Adds a plaintext upstream reachable at `address`.
    pub(crate) fn upstream(mut self, name: &str, address: SocketAddr) -> Self {
        self.upstreams.push(UpstreamEntry::plaintext(
            name,
            address.ip().to_string(),
            address.port(),
        ));
        self
    }

    /// Adds an `https` upstream at `host`:`port`.
    ///
    /// `host` is written as given, so a scenario can name the host a certificate is signed for
    /// (`localhost`) or an address it is not (`127.0.0.1`). Resolution happens at request time,
    /// exactly as it does for a written document.
    pub(crate) fn tls_upstream(mut self, name: &str, host: &str, port: u16) -> Self {
        self.upstreams.push(UpstreamEntry::tls(name, host, port));
        self
    }

    /// Adds a plaintext upstream with an explicit member list on one logical host.
    ///
    /// `host` is the identity every member is addressed by; each member is `(name, address)` and
    /// connects to its own address and port. This is the multi-instance form, and the only one whose
    /// requests record an `upstream.endpoint`.
    pub(crate) fn upstream_with_members(
        mut self,
        name: &str,
        host: &str,
        members: &[(&'static str, SocketAddr)],
    ) -> Self {
        self.upstreams
            .push(UpstreamEntry::members(name, "http", host, members));
        self
    }

    /// Adds an `https` upstream with an explicit member list on one logical host.
    ///
    /// The certificate must name `host`, not the members: a fleet of machines is one service.
    pub(crate) fn tls_upstream_with_members(
        mut self,
        name: &str,
        host: &str,
        members: &[(&'static str, SocketAddr)],
    ) -> Self {
        self.upstreams
            .push(UpstreamEntry::members(name, "https", host, members));
        self
    }

    /// Sets the selection rule of the upstream added last.
    ///
    /// A builder step on the collection rather than an argument to every constructor: the strategy is
    /// a property of one entry, and only the scenarios that assert on selection write one.
    pub(crate) fn strategy(mut self, strategy: &'static str) -> Self {
        let upstream = self
            .upstreams
            .pop()
            .expect("a strategy applies to an upstream that was just added");
        self.upstreams.push(upstream.with_strategy(strategy));
        self
    }

    /// Sets the failure budget of the upstream added last.
    ///
    /// `max_fails` of `0` disables ejection, which is the document's own spelling; a scenario that
    /// wants a member to leave rotation after one failure writes `1`.
    pub(crate) fn ejection(mut self, max_fails: u32, fail_timeout_ms: u32) -> Self {
        let upstream = self
            .upstreams
            .pop()
            .expect("a failure budget applies to an upstream that was just added");
        self.upstreams
            .push(upstream.with_ejection(max_fails, fail_timeout_ms));
        self
    }

    /// Enables the active health check of the upstream added last.
    pub(crate) fn health_check(
        mut self,
        path: &str,
        expect: &str,
        interval_ms: u32,
        timeout_ms: u32,
        unhealthy_threshold: u8,
        healthy_threshold: u8,
    ) -> Self {
        let upstream = self
            .upstreams
            .pop()
            .expect("a health check applies to an upstream that was just added");
        self.upstreams.push(upstream.with_health_check(
            path,
            expect,
            interval_ms,
            timeout_ms,
            unhealthy_threshold,
            healthy_threshold,
        ));
        self
    }
}
