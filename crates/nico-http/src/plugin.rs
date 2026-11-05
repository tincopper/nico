//! The plugin contract: one interface, five phases, two ways to load it.
//!
//! A plugin is a named policy a configuration document attaches to every request (global scope) or to
//! one route. The built-in kinds — JWT, API key, rate limit — are compiled into the binary by
//! `nico-plugins`; a WASM host will wrap a module in the same trait in a later milestone. Neither
//! source changes this module: the document's `kind` is a string, and [`PluginRegistry`] is the only
//! place a string becomes an implementation.
//!
//! # What a plugin may see
//!
//! Only [`PluginContext`], a closed method set whose values are owned and representable everywhere —
//! `String`, `Vec<u8>`, `u16`, `bool`, `u64`. There is no borrowed view of the request, no header-map
//! type and no session handle, because a guest module has no way to hand a borrow back and because a
//! framework type would put the HTTP stack's version into every plugin. Adding one method to the trait
//! is therefore a contract change: the WASM mirror has to grow with it, so the list stays short.
//!
//! # What a plugin may do
//!
//! - **continue**, letting the next plugin and eventually the upstream run;
//! - **refuse**, which is [`PluginError::Refused`] and becomes a typed refusal through the one mapping
//!   in [`HttpError`](crate::HttpError) — never a `502`, never a response the plugin wrote;
//! - **answer**, which is [`Outcome::Respond`] — a complete response for a question that is neither a
//!   failure nor a forward (a preflight, a mock, a cache hit). The data plane cleans it and writes it.
//!
//! A plugin never writes: it produces data, and the one writer decides the wire. That is what keeps
//! `content-length` consistent with the body it describes and keeps the response header set closed.
//!
//! # Phases
//!
//! Request: `on_request` → `on_request_body` → upstream. Response: `on_response` →
//! `on_response_body`. Then `on_logging`, after the request is over. Every stage has a default
//! implementation that continues, so a plugin implements only the stages it uses — the Rust form of
//! "an unset hook pointer".
//!
//! The driver, not the plugin, owns the order and the budget: a stage runs inside the request's
//! remaining time, and a stage that outlives it is terminated by the data plane. A plugin must not
//! start work that outlives its call.

pub(crate) mod chain;
pub(crate) mod context;
mod error;
pub(crate) mod header;
mod phases;
mod registry;
pub(crate) mod response;

use async_trait::async_trait;
pub use chain::{PluginChain, PluginEntry};
pub use error::{LogLevel, PluginError};
pub use phases::PluginPhases;
pub use registry::{PluginBuildError, PluginFactory, PluginPhase, PluginRegistry, RegistryError};
pub use response::{ResponseData, ResponseError};

#[cfg(test)]
mod tests;

/// What one plugin stage produced.
///
/// The set may grow, so matches outside this crate must name the outcomes they handle plus a
/// wildcard.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Outcome {
    /// Let the request (or response) continue unchanged.
    Continue,
    /// Answer the request here: the data plane cleans the response and writes it instead of
    /// forwarding. A plugin must not use it to report a failure — that is [`PluginError::Refused`].
    Respond(ResponseData),
}

/// The result of one plugin stage.
pub type PluginResult = Result<Outcome, PluginError>;

/// One HTTP policy, shared by every request that runs it.
///
/// A plugin is built once, when a document is compiled, and called concurrently from every request
/// that carries it. It is therefore `Send + Sync` and stateless per request: anything that belongs to
/// one request lives in [`PluginContext`], and anything that outlives a request (a token bucket, a
/// cached key set) needs its own synchronisation and must stay bounded.
#[async_trait]
pub trait Plugin: Send + Sync {
    /// Returns the instance name the document gave this entry.
    ///
    /// It is unique across the document, is what a refusal and a log line are attributed to, and is
    /// part of a rate-limit key — so it identifies *this* entry, never the kind.
    fn name(&self) -> &str;

    /// Returns what an operator can learn about this plugin before enabling it.
    ///
    /// `info` is static data: a version, a one-line description, the default configuration a document
    /// may copy, and the optional long-form readme.
    fn info(&self) -> PluginInfo;

    /// Runs before routing (global scope) or right after it (route scope), on request headers only.
    ///
    /// This is the only stage in which [`PluginContext::set_uri`] works. A plugin that refuses here
    /// stops the request before any upstream is chosen.
    ///
    /// # Errors
    /// Returns [`PluginError::Refused`] to answer a typed refusal, or [`PluginError::Execute`] when
    /// the plugin itself failed. The data plane maps both; neither reaches the caller as a `502`.
    async fn on_request(&self, _ctx: &mut dyn PluginContext) -> PluginResult {
        Ok(Outcome::Continue)
    }

    /// Runs once per request-body chunk, or once for the whole body when the document declared
    /// `buffers_body` and the route is therefore buffered.
    ///
    /// `ctx.body_complete()` distinguishes the two: it is `true` only for the last chunk of a
    /// streamed body, and always `true` in the buffered mode.
    ///
    /// # Errors
    /// As [`Plugin::on_request`].
    async fn on_request_body(&self, _ctx: &mut dyn PluginContext) -> PluginResult {
        Ok(Outcome::Continue)
    }

    /// Runs on the response status and headers, before anything is written downstream.
    ///
    /// # Errors
    /// As [`Plugin::on_request`]. A refusal here is still a response, because nothing has been
    /// written yet.
    async fn on_response(&self, _ctx: &mut dyn PluginContext) -> PluginResult {
        Ok(Outcome::Continue)
    }

    /// Runs once per response-body chunk.
    ///
    /// The response is always streamed: an event stream may never end, so no document can declare
    /// that a plugin needs the whole body. `ctx.body_complete()` reports the last chunk.
    ///
    /// This stage is **synchronous**, unlike the other four: the framework hook that delivers a
    /// downstream body chunk cannot await, so a plugin may not either. Nothing here may block either
    /// — the call is on the streaming path, once per chunk — and the work a plugin does must be
    /// bounded by the chunk it is handed.
    ///
    /// A plugin that rewrites bytes here must declare [`PluginPhases::RESPONSE_BODY`] in its
    /// [`PluginInfo`]: that declaration is what tells the data plane the response can no longer be
    /// framed by the length the upstream declared.
    ///
    /// # Errors
    /// As [`Plugin::on_request`]. A failure after the response head was sent cannot be answered, so
    /// the data plane ends the stream and records the request as cancelled.
    fn on_response_body(&self, _ctx: &mut dyn PluginContext) -> PluginResult {
        Ok(Outcome::Continue)
    }

    /// Runs after the request is over: the response is written, the outcome is known.
    ///
    /// This stage cannot change anything, so it returns nothing and a failure inside it is a log
    /// line, not a response. It exists for accounting: metrics, audit trails, "this credential was
    /// used for this request".
    async fn on_logging(&self, _ctx: &mut dyn PluginContext) {}
}

/// Static facts about one plugin implementation, for documentation and operators.
///
/// Every string is `&'static str`: the values are compiled into the implementation, and
/// `default_config` deliberately stays JSON *text* so this crate needs no JSON library to describe a
/// plugin it does not own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginInfo {
    version: &'static str,
    description: &'static str,
    default_config: &'static str,
    readme: Option<&'static str>,
    phases: PluginPhases,
}

impl PluginInfo {
    /// Builds the three facts every plugin has.
    ///
    /// `default_config` must be JSON text: a minimal configuration the kind accepts, which for a kind
    /// with no required field is also what a document that writes no `config` gets. The registry
    /// exposes the same string through [`PluginFactory::default_config`], and a test asserts the two
    /// agree, so the example an operator reads cannot drift from the one the kind loads.
    ///
    /// The phases default to [`PluginPhases::REQUEST`], the stage every chain entry is asked first:
    /// declaring less would claim a plugin does nothing, and the one declaration with a consequence
    /// outside the plugin — [`PluginPhases::RESPONSE_BODY`] — is exactly the one a body-rewriting kind
    /// has to add.
    #[must_use]
    pub const fn new(
        version: &'static str,
        description: &'static str,
        default_config: &'static str,
    ) -> Self {
        Self {
            version,
            description,
            default_config,
            readme: None,
            phases: PluginPhases::REQUEST,
        }
    }

    /// Returns a copy with the long-form readme attached.
    #[must_use]
    pub const fn with_readme(self, readme: &'static str) -> Self {
        Self {
            readme: Some(readme),
            ..self
        }
    }

    /// Returns a copy declaring the stages this plugin implements.
    ///
    /// The declaration does not decide whether a stage is called — every stage is — but it does
    /// decide how the response is framed when it includes [`PluginPhases::RESPONSE_BODY`]. A kind
    /// that rewrites response bytes and omits the flag produces a response whose declared length no
    /// longer matches its body.
    #[must_use]
    pub const fn with_phases(self, phases: PluginPhases) -> Self {
        Self { phases, ..self }
    }

    /// Returns the plugin's version, as its implementation names it.
    #[must_use]
    pub const fn version(&self) -> &'static str {
        self.version
    }

    /// Returns the one-line description of what the plugin does.
    #[must_use]
    pub const fn description(&self) -> &'static str {
        self.description
    }

    /// Returns the default configuration as JSON text.
    #[must_use]
    pub const fn default_config(&self) -> &'static str {
        self.default_config
    }

    /// Returns the long-form readme, when the plugin ships one.
    #[must_use]
    pub const fn readme(&self) -> Option<&'static str> {
        self.readme
    }

    /// Returns the stages this plugin declares it implements.
    #[must_use]
    pub const fn phases(&self) -> PluginPhases {
        self.phases
    }
}

/// Everything one plugin stage is allowed to touch.
///
/// The method set is closed. Two rules apply to all of it:
///
/// - **A phase only offers its own facts.** A method called outside the stage that owns it returns
///   [`PluginError::PhaseViolation`] — never a panic and never a silent no-op. The stage of every
///   method is stated on the method.
/// - **Values are owned.** Bodies are `Vec<u8>`, header values are `String`, and the caller's address
///   is only a `u64` bucket key. Nothing here borrows from the request, so the same list can be
///   mirrored by a WASM guest, and no plugin can hold a reference into state the data plane is about
///   to rewrite.
///
/// A plugin may not write a response through this context: a stage either continues, refuses with a
/// [`PluginError`], or answers with [`Outcome::Respond`].
///
/// `Send` is part of the contract because a plugin stage is an `async fn` on a request task: the
/// context crosses an `.await`, so a stage that holds it in a `Send` future (as every stage does)
/// cannot be handed a `!Send` context. It does not require `Sync`: one stage owns one context.
pub trait PluginContext: Send {
    /// Returns this plugin instance's name, for its own log lines and its identity source.
    fn plugin_name(&self) -> &str;

    /// Returns the request's correlation id; also the `trace_id` of an error body.
    fn trace_id(&self) -> &str;

    /// Returns the request method as sent, for example `GET`.
    fn method(&self) -> String;

    /// Returns the request target as sent: path and query, unchanged.
    fn uri(&self) -> String;

    /// Replaces the request target. Only `on_request` offers this; the data plane reconciles the
    /// rewritten target with the upstream it connects to.
    ///
    /// # Errors
    /// - [`PluginError::PhaseViolation`] outside `on_request`.
    /// - [`PluginError::Execute`] when `uri` is not a valid request target.
    fn set_uri(&mut self, uri: &str) -> Result<(), PluginError>;

    /// Reports whether this is an event stream, from the response content type when one exists and
    /// from the request's `accept` header before that.
    fn is_sse(&self) -> bool;

    /// Reports whether the request asked to upgrade to a WebSocket.
    fn is_websocket(&self) -> bool;

    /// Returns the first value of a request header, or `None` when it is absent.
    ///
    /// A value that is not valid UTF-8 reads as absent: the contract has no byte-string getter, and a
    /// mangled copy of a credential must never be what a plugin authenticates.
    ///
    /// # Errors
    /// [`PluginError::PhaseViolation`] outside `on_request` and `on_request_body`.
    fn request_header(&self, name: &str) -> Result<Option<String>, PluginError>;

    /// Replaces every value of a request header.
    ///
    /// # Errors
    /// [`PluginError::PhaseViolation`] outside the request stages; [`PluginError::Execute`] when the
    /// name is not a token or the value is not visible ASCII, space or tab.
    fn set_request_header(&mut self, name: &str, value: &str) -> Result<(), PluginError>;

    /// Adds one more value to a request header, keeping the existing ones.
    ///
    /// # Errors
    /// As [`PluginContext::set_request_header`].
    fn append_request_header(&mut self, name: &str, value: &str) -> Result<(), PluginError>;

    /// Removes every value of a request header; removing an absent header is not an error.
    ///
    /// # Errors
    /// [`PluginError::PhaseViolation`] outside the request stages; [`PluginError::Execute`] when the
    /// name is not a token.
    fn remove_request_header(&mut self, name: &str) -> Result<(), PluginError>;

    /// Returns the response status.
    ///
    /// # Errors
    /// [`PluginError::PhaseViolation`] before a response exists, i.e. outside `on_response`,
    /// `on_response_body` and `on_logging`.
    fn status(&self) -> Result<u16, PluginError>;

    /// Returns the first value of a response header, or `None` when it is absent.
    ///
    /// # Errors
    /// As [`PluginContext::status`].
    fn response_header(&self, name: &str) -> Result<Option<String>, PluginError>;

    /// Replaces every value of a response header.
    ///
    /// The three mutators below are only available in `on_response`: that is the last moment the head
    /// can still be changed, because the body stages describe a head the client is already reading.
    /// Asking later is a [`PluginError::PhaseViolation`], which says so, rather than an edit that
    /// quietly reaches nothing.
    ///
    /// # Errors
    /// [`PluginError::PhaseViolation`] outside `on_response`; [`PluginError::Execute`] when the name is
    /// not a token or the value is not visible ASCII, space or tab.
    fn set_response_header(&mut self, name: &str, value: &str) -> Result<(), PluginError>;

    /// Adds one more value to a response header, keeping the existing ones.
    ///
    /// # Errors
    /// As [`PluginContext::set_response_header`].
    fn append_response_header(&mut self, name: &str, value: &str) -> Result<(), PluginError>;

    /// Removes every value of a response header; removing an absent header is not an error.
    ///
    /// # Errors
    /// [`PluginError::PhaseViolation`] outside `on_response`; [`PluginError::Execute`] when the name is
    /// not a token.
    fn remove_response_header(&mut self, name: &str) -> Result<(), PluginError>;

    /// Returns the current request-body chunk.
    ///
    /// In the buffered mode this is the whole body, delivered in one call.
    ///
    /// # Errors
    /// [`PluginError::PhaseViolation`] outside `on_request_body`.
    fn request_body(&self) -> Result<Vec<u8>, PluginError>;

    /// Replaces the current request-body chunk, or the whole body in the buffered mode.
    ///
    /// # Errors
    /// [`PluginError::PhaseViolation`] outside `on_request_body`.
    fn set_request_body(&mut self, body: Vec<u8>) -> Result<(), PluginError>;

    /// Returns the current response-body chunk.
    ///
    /// # Errors
    /// [`PluginError::PhaseViolation`] outside `on_response_body`.
    fn response_body(&self) -> Result<Vec<u8>, PluginError>;

    /// Replaces the current response-body chunk.
    ///
    /// # Errors
    /// [`PluginError::PhaseViolation`] outside `on_response_body`.
    fn set_response_body(&mut self, body: Vec<u8>) -> Result<(), PluginError>;

    /// Reports whether the chunk just delivered ends the body.
    ///
    /// Always `true` in the buffered request mode, `true` on the last chunk of a streamed body.
    ///
    /// # Errors
    /// [`PluginError::PhaseViolation`] outside a body stage.
    fn body_complete(&self) -> Result<bool, PluginError>;

    /// Returns the matched route id, or `None` when nothing matched yet.
    ///
    /// A global plugin runs before routing, so it sees `None`. That is the reason a rate limit keyed
    /// by subject has to live in a route scope.
    fn route_id(&self) -> Option<&str>;

    /// Returns the name the route targets: an upstream name or a provider name.
    ///
    /// `None` in a global stage, for the same reason as [`PluginContext::route_id`].
    fn target(&self) -> Option<&str>;

    /// Returns this instance's configuration as JSON text, validated at publication.
    ///
    /// The kind's factory owns the structure; this crate only carries the text, so the request path
    /// never re-parses configuration.
    fn config(&self) -> &str;

    /// Records who the request authenticated as, so a later plugin can key on it.
    ///
    /// The first identity wins: a second write is ignored, not an error, which keeps a later plugin
    /// from replacing the subject a rate limit is already charged to. The source of the identity is
    /// this plugin's own name.
    ///
    /// # Errors
    /// Currently never fails; the `Result` keeps the write uniform with the other mutators.
    fn set_identity(&mut self, subject: &str) -> Result<(), PluginError>;

    /// Returns the subject an earlier plugin authenticated, when one did.
    ///
    /// `None` means "not authenticated", which is different from "authenticated as nobody".
    fn identity(&self) -> Option<&str>;

    /// Returns a stable hash of the caller's address, for bucketing only.
    ///
    /// The address itself is never exposed. The hash is FNV-1a, not a cryptographic digest: it must be
    /// stable within a generation and cheap to compute, and it never leaves the process.
    fn client_ip_hash(&self) -> Option<u64>;

    /// Writes one line attributed to this plugin.
    ///
    /// Plugin lines do not appear in the request log, which is written once by the data plane; this is
    /// how a plugin reports something only it knows (a key rotation, a policy decision) without
    /// corrupting the request record.
    fn log(&mut self, level: LogLevel, message: &str);
}
