//! What a front-end is handed: one client request, and the bytes it wants sent instead.

use std::fmt;

/// The client request one conversion decodes, as a framework-free view.
///
/// Borrowed rather than owned because it is short-lived: it exists between the end of the client
/// body and the encode that produces the provider's bytes. The body is the buffered whole, so the
/// bytes are the caller's message and not a chunk boundary — a decoder may therefore parse it as one
/// document.
///
/// Header values that are not UTF-8 are omitted: a codec reads text, and handing it a mangled copy of
/// a credential is worse than telling it the header is absent. The body is never dropped, because it
/// is the request.
///
/// The values are the client's, so they may include its own credential. They are handed to the
/// front-end to be read, never to be logged: this type's [`Debug`] prints names and lengths.
pub struct LlmRequest<'a> {
    method: &'a str,
    path: &'a str,
    headers: Vec<(&'a str, &'a str)>,
    body: &'a [u8],
}

impl<'a> LlmRequest<'a> {
    /// Builds the view the data plane hands to [`ConvertingRoute::prepare`](super::ConvertingRoute::prepare).
    ///
    /// `headers` holds the pairs whose value is UTF-8, in the order the client sent them; a repeated
    /// name appears once per value. The constructor is public because a front-end's own tests need to
    /// build the view without a server; it establishes no invariant, it only names the value.
    #[must_use]
    pub fn new(
        method: &'a str,
        path: &'a str,
        headers: Vec<(&'a str, &'a str)>,
        body: &'a [u8],
    ) -> Self {
        Self {
            method,
            path,
            headers,
            body,
        }
    }

    /// Returns the client's request method.
    #[must_use]
    pub fn method(&self) -> &'a str {
        self.method
    }

    /// Returns the client's path, without the query string.
    #[must_use]
    pub fn path(&self) -> &'a str {
        self.path
    }

    /// Returns every header whose value is UTF-8, in the client's order.
    #[must_use]
    pub fn headers(&self) -> &[(&'a str, &'a str)] {
        &self.headers
    }

    /// Returns the first value of `name`, compared case-insensitively.
    ///
    /// The first value rather than the last: a client that sends the same header twice has said two
    /// things, and the protocol rules that resolve them belong to the codec, not to a lookup.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&'a str> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| *value)
    }

    /// Returns the buffered client body, whole.
    #[must_use]
    pub fn body(&self) -> &'a [u8] {
        self.body
    }
}

impl fmt::Debug for LlmRequest<'_> {
    /// Prints the method, the path, the header names and the body length — never a header value or a
    /// byte of the body, which are the caller's credential and prompt.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(name, _)| *name).collect();
        formatter
            .debug_struct("LlmRequest")
            .field("method", &self.method)
            .field("path", &self.path)
            .field("headers", &names)
            .field("body_len", &self.body.len())
            .finish()
    }
}

/// The bytes one conversion wants sent to the provider, plus the two facts the request record needs.
///
/// Built by the front-end inside [`ConvertingRoute::prepare`](super::ConvertingRoute::prepare) and consumed by the
/// data plane once: the body becomes the upstream request, `streaming` decides whether the response is
/// translated as an event stream, and `model` is reported and never parsed.
pub struct LlmPrepared {
    body: Vec<u8>,
    streaming: bool,
    model: Option<String>,
}

impl LlmPrepared {
    /// Builds a prepared request from the bytes to send and the stream flag the client asked for.
    #[must_use]
    pub fn new(body: Vec<u8>, streaming: bool) -> Self {
        Self {
            body,
            streaming,
            model: None,
        }
    }

    /// Attaches the model label the request record reports.
    ///
    /// Free-form on purpose: a front-end may report the physical model, or both names — this crate
    /// only writes the value into a log field.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Returns the bytes to send to the provider.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// Takes the bytes to send, leaving the value unusable for a second read.
    #[must_use]
    pub fn into_body(self) -> Vec<u8> {
        self.body
    }

    /// Returns whether the client asked for a streamed answer.
    ///
    /// The data plane does not interpret it: it only passes the flag to
    /// [`ConvertingRoute::transform`](super::ConvertingRoute::transform) and records it, because whether a stream is
    /// expected is a fact of the client's request, which only the front-end decoded.
    #[must_use]
    pub const fn streaming(&self) -> bool {
        self.streaming
    }

    /// Returns the model label the request record reports, when the front-end supplied one.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }
}

impl fmt::Debug for LlmPrepared {
    /// Prints the body's length and never the body: it is the caller's prompt.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LlmPrepared")
            .field("body_len", &self.body.len())
            .field("streaming", &self.streaming)
            .field("model", &self.model)
            .finish()
    }
}
