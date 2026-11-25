//! Route-specific proxy settings and per-response buffering state.

use std::sync::Arc;

use nico_core::{ProxySetHeader, Route};

/// Request-header rewrites cached for one matched route.
#[derive(Debug, Default)]
pub(super) struct ProxyHeaders {
    set: Option<Arc<[ProxySetHeader]>>,
    delete: Option<Arc<[String]>>,
}

impl ProxyHeaders {
    pub(super) fn bind(&mut self, route: &Route) {
        self.set = Some(route.proxy_set_header().to_vec().into());
        self.delete = Some(route.proxy_del_header().to_vec().into());
    }

    pub(super) fn set(&self) -> &[ProxySetHeader] {
        self.set.as_deref().unwrap_or(&[])
    }

    pub(super) fn delete(&self) -> &[String] {
        self.delete.as_deref().unwrap_or(&[])
    }
}

#[derive(Debug)]
pub(super) struct ResponseSpool {
    accel_passthrough: Option<bool>,
    bytes: Vec<u8>,
    degraded: bool,
    enabled: bool,
    max_bytes: usize,
}

impl Default for ResponseSpool {
    fn default() -> Self {
        Self {
            accel_passthrough: None,
            bytes: Vec::new(),
            degraded: false,
            enabled: true,
            max_bytes: 1024 * 1024,
        }
    }
}

impl ResponseSpool {
    pub(super) fn set_route(&mut self, enabled: bool, max_bytes: u32) {
        self.enabled = enabled;
        self.max_bytes = max_bytes as usize;
    }

    pub(super) fn capture_accel_buffering(&mut self, passthrough: Option<bool>) {
        self.accel_passthrough = passthrough;
    }

    pub(super) fn should_buffer(&self) -> bool {
        self.enabled && self.accel_passthrough != Some(true) && !self.degraded
    }

    pub(super) fn push(&mut self, chunk: &[u8]) -> bool {
        if self.degraded {
            return false;
        }
        if self.bytes.len().saturating_add(chunk.len()) > self.max_bytes {
            self.degraded = true;
            return true;
        }
        self.bytes.extend_from_slice(chunk);
        false
    }

    pub(super) fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.bytes)
    }
}

impl super::HttpContext {
    pub(crate) fn proxy_set_header(&self) -> &[ProxySetHeader] {
        self.proxy_headers.set()
    }

    pub(crate) fn proxy_del_header(&self) -> &[String] {
        self.proxy_headers.delete()
    }

    pub(crate) fn capture_accel_buffering(&mut self, passthrough: Option<bool>) {
        self.proxy.capture_accel_buffering(passthrough);
    }

    pub(crate) fn response_buffering_for_this(&self) -> bool {
        self.proxy.should_buffer()
    }

    pub(crate) fn spool_chunk(&mut self, chunk: &[u8]) -> bool {
        self.proxy.push(chunk)
    }

    pub(crate) fn take_spool(&mut self) -> Vec<u8> {
        self.proxy.take()
    }
}

#[cfg(test)]
mod tests {
    use super::ResponseSpool;

    #[test]
    fn overflow_chunk_is_not_retained_and_degradation_stops_buffering() {
        let mut spool = ResponseSpool::default();
        spool.set_route(true, 3);

        assert!(!spool.push(b"ab"));
        assert!(spool.push(b"cd"));
        assert!(!spool.should_buffer());
        assert_eq!(spool.take(), b"ab");
        assert!(!spool.push(b"ef"));
        assert!(spool.take().is_empty());
    }

    #[test]
    fn origin_can_disable_route_buffering_for_one_response() {
        let mut spool = ResponseSpool::default();
        spool.set_route(true, 3);
        spool.capture_accel_buffering(Some(true));
        assert!(!spool.should_buffer());
    }
}
