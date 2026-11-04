//! Per-request state: the one snapshot a request sees, the peer it goes to, and what it costs.
//!
//! [`HttpContext`] is the whole world an [`HttpFilter`](crate::HttpFilter) is allowed to see. It
//! deliberately exposes neither the session nor the request body: a filter cannot write a response
//! behind the data plane's back, and cannot consume body bytes that Pingora will not replay. What a
//! filter *can* do is record where the request goes ([`HttpContext::set_destination`]), which is how
//! a policy that understands a provider target hands it to the data plane.
//!
//! The context copies the few fields it needs — route id, target name, method, path — instead of
//! borrowing them from the document. A borrow would make the context self-referential, because it
//! also owns the `Arc<RuntimeSnapshot>` that keeps those values alive; copying a handful of small
//! strings per request is the cheaper trade.
//!
//! Fields mean different things and each has its own rule (a deadline saturates at zero, a body
//! counter only grows, an error slot keeps the first failure), so they are addressed by named
//! accessors rather than by one field enum: there is no shared table of names to keep in sync.
//!
//! The budget is owned here, so the two facts derived from it are too: [`HttpContext::expired`] and
//! [`HttpContext::deadline_error`]. Every hook that must stop when time is up asks the context
//! instead of re-deriving the comparison from `deadline()` and reporting its own elapsed time.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use nico_core::{Generation, Route, RouteTarget, RuntimeSnapshot};
use pingora::http::{Method, RequestHeader};

use crate::{error::HttpError, limits, upstream::Destination};

/// What the data plane remembers about one in-flight request.
///
/// Built empty by `new_ctx`, filled by [`HttpContext::begin`] once a route has matched, then read by
/// the hooks that follow. A request that never matched a route keeps an empty context, which is why
/// every accessor is optional.
#[derive(Debug, Default)]
pub struct HttpContext {
    snapshot: Option<Arc<RuntimeSnapshot>>,
    route_id: Option<String>,
    target: Option<RouteTarget>,
    method: Option<Method>,
    path: Option<String>,
    started: Option<Instant>,
    deadline: Option<Instant>,
    destination: Option<Destination>,
    body_limit: Option<u32>,
    body_bytes: u64,
    error: Option<HttpError>,
    cancelled: bool,
    trace_id: String,
}

impl HttpContext {
    /// Records what the request matched and the budget it may spend.
    ///
    /// Called once per request, as soon as routing has chosen a route; a cancelled or rejected
    /// request never reaches it and keeps its empty context.
    ///
    /// The snapshot is borrowed and cloned into the context rather than taken by value: the caller
    /// passes `&route`, which borrows the same snapshot, so the context has to do the clone.
    ///
    /// `now` is the instant the request started: the deadline is derived from it so a test can
    /// drive the clock instead of sleeping.
    pub fn begin(
        &mut self,
        snapshot: &Arc<RuntimeSnapshot>,
        route: &Route,
        header: &RequestHeader,
        now: Instant,
    ) {
        let limits = snapshot.document().limits();
        self.route_id = Some(route.id().to_owned());
        self.target = Some(route.target().clone());
        self.method = Some(header.method.clone());
        self.path = Some(header.uri.path().to_owned());
        self.started = Some(now);
        self.deadline = Some(now + limits::request_budget(route, limits));
        self.body_limit = Some(limits::body_limit(route, limits));
        self.snapshot = Some(Arc::clone(snapshot));
    }

    /// Returns the snapshot this request is served from, once it has begun.
    ///
    /// The `Arc` is held for the whole request, so the generation cannot change under it.
    #[must_use]
    pub fn snapshot(&self) -> Option<&RuntimeSnapshot> {
        self.snapshot.as_deref()
    }

    /// Returns the generation this request is served from, for logging.
    #[must_use]
    pub fn generation(&self) -> Option<Generation> {
        self.snapshot.as_ref().map(|snapshot| snapshot.generation())
    }

    /// Returns the matched route id, for logging.
    #[must_use]
    pub fn route_id(&self) -> Option<&str> {
        self.route_id.as_deref()
    }

    /// Returns the route's target, which names either an upstream or a provider.
    #[must_use]
    pub fn target(&self) -> Option<&RouteTarget> {
        self.target.as_ref()
    }

    /// Returns the name the route targets: an upstream name or a provider name.
    ///
    /// This is the `upstream` log field: for a provider route it names the provider the request was
    /// handed to, which is what an operator needs to correlate a failure.
    #[must_use]
    pub fn upstream(&self) -> Option<&str> {
        self.target.as_ref().map(RouteTarget::name)
    }

    /// Records where the request goes.
    ///
    /// Two callers exist and neither can be surprising to the other: a filter that owns the target
    /// kind a route named (a provider) resolves it before an upstream is selected, and the data
    /// plane records the destination it derived for a document upstream. Precedence is decided where
    /// the destination is chosen, not here: a destination a filter recorded is never replaced by the
    /// fallback the data plane would otherwise derive.
    pub fn set_destination(&mut self, destination: Destination) {
        self.destination = Some(destination);
    }

    /// Returns where the request goes, once anything has decided it.
    ///
    /// The value is read by the hook that connects and by the one that rewrites `Host`, so both
    /// describe the same peer even when a filter chose it.
    #[must_use]
    pub fn destination(&self) -> Option<&Destination> {
        self.destination.as_ref()
    }

    /// Returns the request method, for logging and filters.
    #[must_use]
    pub fn method(&self) -> Option<&Method> {
        self.method.as_ref()
    }

    /// Returns the request path, for logging and filters. The query string is not part of it.
    #[must_use]
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    /// Returns the whole-request deadline, once the request has begun.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Returns the time left before the deadline, saturating at zero.
    ///
    /// `None` means the request has not begun, so it has no budget yet — different from "expired",
    /// which is `Some(Duration::ZERO)`.
    #[must_use]
    pub fn remaining(&self, now: Instant) -> Option<Duration> {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    /// Reports whether the request has spent its whole budget.
    ///
    /// A context that never began is not expired: an unrouted request is rejected by routing, not
    /// by a clock.
    #[must_use]
    pub fn expired(&self, now: Instant) -> bool {
        self.remaining(now)
            .is_some_and(|remaining| remaining.is_zero())
    }

    /// Builds the failure for an expired budget.
    ///
    /// It lives here because the context owns the clock the elapsed time is measured against, so
    /// every hook that gives up reports the same number for the same request.
    #[must_use]
    pub fn deadline_error(&self) -> HttpError {
        HttpError::DeadlineExceeded {
            elapsed_ms: self.elapsed_ms(Instant::now()),
        }
    }

    /// Returns the milliseconds spent on this request so far, for the `elapsed_ms` log field.
    #[must_use]
    pub fn elapsed_ms(&self, now: Instant) -> u64 {
        self.started.map_or(0, |started| {
            now.saturating_duration_since(started).as_millis() as u64
        })
    }

    /// Returns how many body bytes this route accepts.
    #[must_use]
    pub fn body_limit(&self) -> Option<u32> {
        self.body_limit
    }

    /// Returns how many body bytes have been counted so far.
    #[must_use]
    pub fn body_bytes(&self) -> u64 {
        self.body_bytes
    }

    /// Adds a received body chunk to the count.
    pub fn count_body(&mut self, bytes: usize) {
        self.body_bytes = self
            .body_bytes
            .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
    }

    /// Reports whether the counted body has crossed this route's limit.
    #[must_use]
    pub fn body_over_limit(&self) -> bool {
        self.body_limit
            .is_some_and(|limit| limits::over_limit(limit, self.body_bytes))
    }

    /// Stores a failure for the hook that can still act on it.
    ///
    /// The first failure wins: a later symptom of an earlier cause must not replace it.
    pub fn fail(&mut self, error: HttpError) {
        if self.error.is_none() {
            self.error = Some(error);
        }
    }

    /// Returns the stored failure without taking it, when a hook only needs to inspect it.
    #[must_use]
    pub fn error(&self) -> Option<&HttpError> {
        self.error.as_ref()
    }

    /// Marks the request as cancelled: the client disconnected or shutdown began.
    ///
    /// A cancelled request produces no response. Recording it keeps the logging hook from reporting
    /// a disconnect as a server error.
    pub fn mark_cancelled(&mut self) {
        self.cancelled = true;
    }

    /// Returns whether the request was cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    /// Records the correlation id for this request's log line and error bodies.
    ///
    /// It is written once, before routing, so every later hook — including the ones that answer a
    /// request that matched no route — reports the same value the caller can quote back.
    pub fn set_trace_id(&mut self, trace_id: &str) {
        self.trace_id.clear();
        self.trace_id.push_str(trace_id);
    }

    /// Returns the correlation id, or `""` before one was recorded.
    #[must_use]
    pub fn trace_id(&self) -> &str {
        &self.trace_id
    }
}

#[cfg(test)]
mod tests;
