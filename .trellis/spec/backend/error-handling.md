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
| `nico-core` | `ErrorCategory` (protocol-agnostic classification) | Says *what kind* of failure it is, never what status code to return |
| `nico-config` | `ConfigSourceError` | Source protocol failure, mapped from etcd/Nacos/file errors |
| `nico-http` | `HttpError` | HTTP semantics: bad request, upstream failure, timeout, response already committed |
| `nico-llm` | `ProviderError` | Provider call failure, upstream quota, malformed provider response |
| `nico-server` | `anyhow::Result` in `main` only | Startup/reload orchestration reporting |

Errors wrap, they do not flatten. A layered crate nests the error of a crate it is **allowed** to
depend on (see the dependency direction in `directory-structure.md`) instead of copying its fields:

```rust
// nico-llm may depend on nico-http, so it can nest HttpError.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("provider transport failed: {0}")]
    Transport(#[from] nico_http::HttpError),
    #[error("provider returned malformed stream: {reason}")]
    MalformedStream { reason: String },
    #[error("provider quota exhausted for {provider}")]
    QuotaExhausted { provider: &'static str },
}
```

The reverse direction is forbidden: `nico-http` must not name `ProviderError`, and
`nico-core` must not name any of them. Those layers keep their own error type and rely on
`ErrorCategory` to classify what happened.

---

## Error Types

Libraries use `thiserror`; binaries may use `anyhow`. Both rules are hard:

```rust
// nico-config/src/source.rs
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
of a `nico-*` library crate. `Box<dyn Error + Send + Sync>` **is** allowed as a `#[source]`
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
    #[error("request deadline exceeded after {elapsed_ms} ms")]
    DeadlineExceeded { elapsed_ms: u64 },
    #[error("the request body was still incomplete when the deadline expired")]
    RequestTimeout,
    #[error("upstream failed before response was committed: {source}")]
    UpstreamFailed { #[source] source: Box<dyn std::error::Error + Send + Sync> },
    #[error("response already committed; cannot retry or replace")]
    ResponseCommitted,
}
```

`ResponseCommitted` exists to make retry legality a type-level decision rather than a comment.
Cancellation is **not** a variant: a client disconnect, a shutdown, or a deadline that expires after
the response was committed produce no response at all, so they are an outcome carried in the request
context and logged as `http.status = 499` with `error.category = Cancelled` — never a second
response. A deadline that expires while the caller is still sending a body *is* a variant
(`RequestTimeout`, `408`), because the data plane knows the request never finished arriving and the
peer is not at fault.

### Retry and fallback legality

Retry only when **all** of the following hold, and encode the check in one place:

1. The request is replayable (body fully buffered or seekable, no consumed stream).
2. The operation is idempotent or explicitly retry-safe.
3. No response bytes were committed to the client (`HttpError::ResponseCommitted` was not reached).
4. The retry budget for this request has not been exhausted.

LLM fallback additionally requires model-capability compatibility and duplicate-billing
awareness — see the LLM design notes. Never fall back after the first streamed byte.

### Error categories in the kernel

`nico-core` classifies but does not decide HTTP:

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
category is the only thing that crosses into `nico-core`.

Protocol layers map categories to their own representation (HTTP status, MQTT reason code, gRPC
status). `nico-core` must never contain `StatusCode` or any wire-level detail.

---

## Protocol Error Responses

Mapping happens **once**, at the edge layer:

| Category | HTTP | Notes |
|---|---|---|
| `Client` (bad input) | 400 | Include a stable machine-readable error code, not internal detail |
| `Client` (incomplete request) | 408 | The caller stopped sending before its budget expired |
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

### Authentication refusals

A policy that refuses a caller does not get its own mapper. A plugin returns
`PluginError::Refused { rejection, .. }`, the one `From<PluginError> for HttpError` turns it into
`HttpError::Rejected`, and `HttpError::respond` turns that into the same `Respond` a built-in failure
produces. Every other `PluginError` — `Execute`, `PhaseViolation` — is an `Internal` and answers
`500`. `HttpError::Internal` keeps a caller-safe `Display` ("internal data-plane failure") and its
cause names the plugin layer; the instance name is carried as a `plugin` field on the one request
record. Invariant: a `500` attributable to a plugin must let an operator identify the instance
without putting internal detail in the response body. Two codes are stable for the whole
authentication family:

| Code | Status | When |
|---|---|---|
| `auth_missing` | 401 + `www-authenticate` | The request presented no credential at all |
| `auth_invalid` | 401 + `www-authenticate` | A credential was presented and is not acceptable |

`auth_invalid` is deliberately **not** split further. Expired, unsigned, wrong signature, unknown
`kid`, unknown API key and revoked key all answer it, because a caller that can tell them apart can
enumerate which keys exist. The cause is a fixed sentence that reaches the log and nothing else; it
must never quote the credential, and a test asserts that the error's `Display` does not contain it.

The pair is one code per **decision this layer makes** ("is there a credential" versus "may this
credential in"), not one per reason a credential can fail.

### LLM provider errors (`nico-llm`)

Two independent axes meet in one exchange: a client protocol and a provider. `CanonicalError` is the
only failure value both sides share — it is not any vendor's error object. A provider adapter turns a
provider status and body into it; a client codec renders it in the client protocol's error envelope.
The provider body, headers and credential never reach the client or the request record, and the
adapter's parser diagnostic stays in the log.

The codes below are the gateway's, not a provider's. Each is a compile-time constant owned by
`nico-llm`'s exchange module: renaming one is a breaking change, and none is configurable. A
provider's own code travels in `CanonicalError::provider_code` for structured logs only.

| Source | Canonical code | Status | Decision |
|---|---|---|---|
| the request needs a capability the `(client protocol, provider)` pair does not share | `unsupported_capability` | 422 | the document is well formed and the route cannot serve it |
| the client codec cannot read the client document | `invalid_request` | 400 | the caller's document is malformed; the diagnostic names a field, never the payload |
| the client codec cannot encode the canonical answer | `client_encode_failed` | 500 | gateway defect |
| the provider adapter cannot encode the canonical request | `provider_encode_failed` | 500 | gateway defect |
| the route's base URL or credential makes an unusable endpoint | `upstream_endpoint_invalid` | 500 | configuration defect; refused at publication when it is static |
| the provider's bytes are not a document of its own protocol | `upstream_protocol_error` | 502 | the peer failed |
| the provider answered a failure status | the adapter's `CanonicalError` | the adapter's status | the provider body is not forwarded |
| a canonical invariant fails after a codec or adapter produced a value | `internal_error` | 500 | gateway defect: a value crossed the boundary without its constructors |

- **422 is a deliberate refinement of the `Client` row above.** The request is well formed, so `400`
  would tell the caller to fix a document with nothing wrong in it. The entry is protocol-neutral —
  it names a capability, not a wire field — so every codec renders it without translating.
- **The status of a failure belongs to the canonical error.** `ClientCodec::encode_error` therefore
  returns only the body, and the exchange takes the status from `CanonicalError::http_status`.
- **The status of a success belongs to the client codec.** `ClientCodec::encode_response` returns a
  `ClientReply { status, body }`: a complete answer is a success in the client's own vocabulary, and
  a provider's 2xx variant describes that provider's framing, not the caller's request. Forwarding it
  would put a provider detail into the one header the caller reads.
- **Nothing is retried after the first streamed byte.** The canonical error carries `retryable` for a
  retry budget above the exchange; once a byte reached the caller, a failure ends the stream and no
  second status is written.
- **`CanonicalError` validates on construction.** Its crate-private `from_static_parts` constructor
  exists for the constant table above and skips validation, which is why the mapping's tests call
  `validate()` on every value the table can produce. It never receives caller or provider input.
- **The category travels as its lowercase name.** That spelling is `ErrorCategory`'s serde form, a
  diagnostic/fixture representation rather than a protocol status, and it is pinned by a test in
  `nico-core` so renaming a variant cannot silently change a fixture or a log record.

A capability the pair does not support is refused at publication when it is configuration, and at
request time as a typed client error when it depends on the body — never silently dropped.

### What a plugin may and may not change

The refusal of one plugin instance is configurable — status, content type and body — and the boundary
is what keeps a configuration from turning a caller's mistake into an observable incident:

| Path | Input / transformation | Writer |
|---|---|---|
| data-plane failure | `HttpError::*` default mapping | `write_respond` |
| plugin refusal, default presentation | `PluginError::Refused(default Rejection)` | `write_respond` |
| plugin refusal, per-instance presentation | `PluginError::Refused(compiled error_response)` | `write_respond` |
| synthesized answer | `Outcome::Respond(ResponseData)` → `clean_response` | `write_synthetic` |

- **Status: 4xx only.** Authentication and a rate limit are the caller's problem. A 5xx would make
  monitoring count user errors as gateway faults and contradict `error.category = Client`; a 2xx is
  not a refusal.
- **The stable code is not configurable.** It travels in the body template *and* in
  `x-nico-error-code`, so a custom body never costs an operator the machine-readable answer.
- **The header set is closed.** `content-type`, `content-length`, `x-nico-error-code`,
  `x-nico-trace-id`, plus `www-authenticate` (authentication) and `retry-after` (a limit). A plugin
  supplies a template and a status, never a header — otherwise one document could desynchronise
  `content-length` from the body it describes.
- **Template variables are an allowlist: `code`, `message`, `trace_id`.** Upstream names,
  configuration paths and credentials have no spelling, so "a refusal never echoes internal detail"
  is a property of the compiler rather than a rule a reviewer has to check.
- **All of it is validated at publication.** Status range, content type, variable names and JSON
  validity are checked while the document is compiled; a failure keeps the last-known-good document
  in service.

A **synthesized response** (`Outcome::Respond`) is not a refusal and is cleaned differently: it is an
answer whose question was neither a failure nor a forward (a preflight, a mock, a cache hit), so it
keeps the end-to-end headers its plugin produced and loses only the ones the data plane owns —
hop-by-hop headers, a `content-length` the plugin supplied (recomputed from the bytes written), and the
two refusal-only headers (`www-authenticate`, `retry-after`). A plugin cannot reach a `ResponseHeader`
in either case: it produces data and `proxy::failure` writes it.

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
