# Error Handling

> How errors are produced, propagated, logged, and turned into protocol responses.

---

## Overview

The gateway is a network proxy: **errors are normal traffic**, not exceptional events. The rules
below exist so that a single bad upstream, a config typo, or a cancellation never turns into a
panic, a leaked internal detail, or a double-logged error.

Layering:

| Layer | Error type | Responsibility |
|---|---|---|
| `gateway-core` | `ErrorCategory` (protocol-agnostic classification) | Says *what kind* of failure it is, never what status code to return |
| `gateway-config` | `ConfigSourceError` | Source protocol failure, mapped from etcd/Nacos/file errors |
| `gateway-http` | `HttpError` | HTTP semantics: bad request, upstream failure, timeout, response already committed |
| `gateway-ai` | `ProviderError` | Provider call failure, upstream quota, malformed provider response |
| `gateway-server` | `anyhow::Result` in `main` only | Startup/reload orchestration reporting |

Errors wrap, they do not flatten. A layered crate nests the error of a crate it is **allowed** to
depend on (see the dependency direction in `directory-structure.md`) instead of copying its fields:

```rust
// gateway-ai may depend on gateway-http, so it can nest HttpError.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("provider transport failed: {0}")]
    Transport(#[from] gateway_http::HttpError),
    #[error("provider returned malformed stream: {reason}")]
    MalformedStream { reason: String },
    #[error("provider quota exhausted for {provider}")]
    QuotaExhausted { provider: &'static str },
}
```

The reverse direction is forbidden: `gateway-http` must not name `ProviderError`, and
`gateway-core` must not name any of them. Those layers keep their own error type and rely on
`ErrorCategory` to classify what happened.

---

## Error Types

Libraries use `thiserror`; binaries may use `anyhow`. Both rules are hard:

```rust
// gateway-config/src/source.rs
#[derive(Debug, thiserror::Error)]
pub enum ConfigSourceError {
    #[error("config source unavailable: {source_name}")]
    Unavailable {
        source_name: &'static str,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("config document exceeds limit: {size} bytes > {limit}")]
    TooLarge { size: usize, limit: usize },

    #[error("config document is not valid UTF-8")]
    NotUtf8,

    #[error("config revision went backwards: current {current:?}, incoming {incoming:?}")]
    RevisionRegression { current: Revision, incoming: Revision },

    #[error("watch stream requires a full reload (compaction or gap)")]
    ResyncRequired,
}
```

Every layer's public error type also exposes `fn category(&self) -> ErrorCategory` when the caller
must classify the failure (`DocumentError`, `ConfigSourceError`, `CacheError` in the stage-0 code).
Mapping lives in one place per layer, so the edge maps a category instead of matching every variant.

Requirements for every public error enum:

- `#[derive(Debug)]` plus `std::error::Error` (via `thiserror`).
- `Send + Sync + 'static` — required for `?` inside async traits and for errors crossing `.await`
  and `tokio::spawn` boundaries. Add a compile-time assertion in a test rather than assuming it.
- One variant per **decision the caller makes**. If two variants always take the same action,
  they are one variant.
- `#[source]` for the underlying cause; `#[from]` only when the conversion is unambiguous
  (1:1 mapping with no extra context).
- Messages are lowercase, no trailing period, and never contain secrets or full payloads.
- `PartialEq` when practical, so tests can assert the exact variant instead of a string.
- A message that quotes a limit is **derived from the constant the check applied**, never written as
  a literal: the check formats it (`format!("must be at most {MAX_HOST_BYTES} bytes")`) and the
  variant carries `Cow<'static, str>`. A hand-typed number in an error string is a second source of
  truth and drifts the moment the constant changes, silently.
- Keep the split explicit: `&'static str` for what names the failure (`field`, `kind`), `Cow` only
  where the text is computed. A fixed sentence stays borrowed, so the common case still allocates
  nothing.

Forbidden in library error types: `Box<dyn std::error::Error>` as the error **type** itself,
`String` as the only error variant (`MyError(String)`), and `anyhow::Error` in any `pub` signature
of a `gateway-*` library crate. `Box<dyn Error + Send + Sync>` **is** allowed as a `#[source]`
field — that is how `thiserror` nests a foreign cause — but never as the return type of a library
function. It is also the correct return type for a `tokio::spawn`ed task boundary.

A single-error module does not need an enum — a `thiserror` struct is idiomatic (fields stay
private, matching the `pub`-field rule in `directory-structure.md`):

```rust
#[derive(Debug, thiserror::Error, PartialEq)]
#[error("upstream returned {code}: {message}")]
pub struct UpstreamError {
    code: u16,
    message: String,
}

impl UpstreamError {
    #[must_use]
    pub fn code(&self) -> u16 { self.code }
}
```

---

## Error Handling Patterns

### Propagation

Use `?` and keep the context chain intact. When the error also needs to be logged, use
`inspect_err` instead of a `match` that logs and re-wraps:

```rust
fn compile(&self, doc: &ConfigVersion) -> Result<RuntimeSnapshot, CompileError> {
    // `yaml::` is a placeholder for the pinned YAML crate; see quality-guidelines.md.
    let resources: ResourcesDoc = yaml::from_slice(&doc.bytes)
        .inspect_err(|err| tracing::warn!(error = %err, "config document parse failed"))
        .map_err(|err| CompileError::Parse { source: err })?;
    let routes = RouteTable::try_new(&resources.routes)?;
    let snapshot = RuntimeSnapshot::new(routes, doc.generation());
    Ok(snapshot)
}
```

When a function has a fixed fallback for the failure case, prefer `let ... else` over a `match`
that only returns an error:

```rust
let Some(peer) = self.peers.get(name) else {
    return Err(HttpError::UnknownUpstream { name: name.to_owned() });
};
```

Do not swallow errors with `let _ = ...`. If a result is intentionally ignored, write the reason:

```rust
// Best-effort metric flush; a failed flush must not abort shutdown.
if let Err(err) = exporter.flush().await {
    tracing::warn!(error = %err, "metric flush failed during shutdown");
}
```

### No panics on the request path

- No `unwrap()` in non-test code; Clippy denies it (`clippy::unwrap_used`).
- `expect()` is denied too (`clippy::expect_used`). Where an invariant is genuinely local and
  provable, opt in explicitly so the exception is visible in review:

```rust
#[expect(clippy::expect_used, reason = "RouteTable::try_new rejects empty tables")]
fn first_route(&self) -> &CompiledRoute {
    self.routes.first().expect("non-empty by construction")
}
```

- No `panic!`, `todo!`, `unimplemented!` in request handling, config compilation, or stream
  parsing. Return an error instead.
- `unreachable!` is denied by Clippy as well (`clippy::unreachable`). The only acceptable form is
  inside a type-state invariant, opted in explicitly and with a comment naming the invariant:

```rust
#[expect(clippy::unreachable, reason = "type-state guarantees handle is Some")]
fn handle_mut(&mut self) -> &mut Handle {
    let Some(handle) = self.handle.as_mut() else {
        // INVARIANT: only reachable from `File<FileOpened>`, whose constructor always sets `handle`.
        unreachable!("File<FileOpened> always has a handle")
    };
    handle
}
```

- No `catch_unwind` as control flow. A panic inside a Pingora worker is a bug, not a retry path.
- `assert!` belongs in tests and debug invariant checks, not in hot paths.

### Async-specific rules

- Never hold a `std::sync::Mutex` guard across `.await`. Use `tokio::sync::Mutex` for
  async-held locks, and prefer lock-free snapshots (`ArcSwap`) over locks on the request path.
- Every `await` that talks to the outside world has a timeout or a deadline derived from the
  request context. A per-read timeout is not a substitute for a whole-call deadline.
- Cancellation is not an error to report as `500`. Model it explicitly:

```rust
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("request cancelled by client or shutdown")]
    Cancelled,
    #[error("request deadline exceeded after {elapsed_ms} ms")]
    DeadlineExceeded { elapsed_ms: u64 },
    #[error("upstream failed before response was committed: {source}")]
    UpstreamFailed { #[source] source: Box<dyn std::error::Error + Send + Sync> },
    #[error("response already committed; cannot retry or replace")]
    ResponseCommitted,
}
```

`ResponseCommitted` exists to make retry legality a type-level decision rather than a comment.

### Retry and fallback legality

Retry only when **all** of the following hold, and encode the check in one place:

1. The request is replayable (body fully buffered or seekable, no consumed stream).
2. The operation is idempotent or explicitly retry-safe.
3. No response bytes were committed to the client (`HttpError::ResponseCommitted` was not reached).
4. The retry budget for this request has not been exhausted.

AI fallback additionally requires model-capability compatibility and duplicate-billing
awareness — see the AI design notes. Never fall back after the first streamed byte.

### Error categories in the kernel

`gateway-core` classifies but does not decide HTTP:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCategory {
    Client,      // caller's fault: bad input, auth failure
    Upstream,    // dependency failed: provider, config center, peer
    Limit,       // resource or rate limit reached
    Internal,    // our bug or invariant violation
    Cancelled,   // deadline / shutdown / client disconnect
}
```

`ErrorCategory` derives `Copy` because it is a five-variant fieldless enum — the textbook `Copy`
case from the handbook: tag-like, all payloads trivially copyable. `Upstream` deliberately does
**not** carry the error value; the concrete error lives in the layer that produced it, and the
category is the only thing that crosses into `gateway-core`.

Protocol layers map categories to their own representation (HTTP status, MQTT reason code, gRPC
status). `gateway-core` must never contain `StatusCode` or any wire-level detail.

---

## Protocol Error Responses

Mapping happens **once**, at the edge layer:

| Category | HTTP | Notes |
|---|---|---|
| `Client` (bad input) | 400 | Include a stable machine-readable error code, not internal detail |
| `Client` (auth) | 401 / 403 | Never echo the presented credential |
| `Limit` | 429 / 413 | Include `Retry-After` only when a real budget is known |
| `Upstream` | 502 / 504 | Distinguish "peer unreachable" from "peer timed out" |
| `Internal` | 500 | Generic body; details go to logs only |
| `Cancelled` | 499 (client disconnect) | Do not log as `error`; do not emit a 500 |

Response body shape (stable, versioned by the API, not by the error type):

```json
{ "error": { "code": "upstream_timeout", "message": "upstream did not respond in time", "trace_id": "..." } }
```

Rules:

- `code` is a stable snake_case string owned by the edge layer; renaming it is a breaking change.
- `message` is safe for external callers. Internal detail (hostnames, config paths, provider
  payloads) never appears here.
- `trace_id` is always present so a caller report can be joined to logs.

---

## Logging Errors

Log an error **once**, at the boundary that can act on it, with structured fields:

```rust
tracing::error!(
    error = %err,
    error.category = ?err.category(),
    route = %route_id,
    upstream = %peer,
    retry_attempt = attempt,
    "upstream request failed"
);
```

Anti-patterns:

- `tracing::error!` inside a `map_err` closure *and* again at the caller (double logging) —
  prefer `.inspect_err(|err| tracing::warn!(...)).map_err(...)` so it happens once.
- Logging the full error chain as a formatted `String` at `debug!` and never at `error!`.
- Logging expected cancellations at `error!` — they belong at `debug!`/`info!`.

### Error testing

Errors are part of the contract, so they get their own tests:

- Derive `PartialEq` where possible and assert the exact variant; otherwise assert `to_string()`.
- Assert the `#[source]` chain is preserved, not just the top-level message.
- Assert that a cancellation or deadline is not reported as `Internal`.
- Assert that a public error's `Display` never contains a secret: a test that formats the error
  after a failed auth and greps for the credential value.

---

## Common Mistakes

| Mistake | Why it hurts | Do instead |
|---|---|---|
| `anyhow` in a library `pub fn` | Callers lose the ability to match on failure | `thiserror` enum per layer |
| `unwrap()` on a config-document parse in config compile | One bad config panics a worker instead of keeping last-known-good | `?` into `CompileError`, keep LKG |
| Mapping every error to `500` | Clients cannot distinguish their bug from ours; retries get misdirected | Map via `ErrorCategory` at the edge |
| Retrying after `commit` | Duplicate bytes to the client / double billing | `HttpError::ResponseCommitted` gate |
| `Mutex` guard held across `.await` | Deadlock or `!Send` future | Snapshot + `ArcSwap`, or `tokio::sync` |
| Timeout only on each read | Slow-drip upstream pins resources forever | Whole-call deadline from `RequestMeta` |
| Error message containing the JWT or API key | Credential leak into logs and responses | Log a hash/prefix, never the value |
| `String`-typed errors (`Err("...".into())`) | Unmatchable, no source chain | Typed variant with `#[source]` |
| Error type missing `Send + Sync + 'static` | `?` across `.await` or in `tokio::spawn` fails to compile | Add the bounds; assert them in a test |
| Logging inside `map_err` and again at the caller | Double logging, noisy alerts | `.inspect_err(...).map_err(...)` once, log at the acting boundary |
| `match` that only returns an error | Verbose and easy to get wrong | `let ... else { return Err(...) }` |
