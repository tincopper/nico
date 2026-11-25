//! Per-route traffic proxy controls used by HTTP integration scenarios.

impl super::Route {
    /// Chooses streaming response delivery instead of buffering.
    pub(crate) fn proxy_buffering_off(mut self) -> Self {
        self.proxy_buffering = Some(false);
        self
    }
}
