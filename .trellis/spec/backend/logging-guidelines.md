# Logging Guidelines

> Structured logging, tracing spans, and metrics conventions.

---

## Overview

- **Library**: `tracing` for both logs and spans. No `log`, `println!`, `eprintln!`, or `dbg!`
  anywhere in `crates/` (enforced by clippy `clippy::print_stdout` / `clippy::print_stderr` /
  `clippy::dbg_macro`).
- **Emit from libraries, configure from the binary**: library crates only create events and spans.
  Subscriber setup, filtering, formatting, and exporters live in `nico-server`.
- **No custom observability framework**: `tracing` spans plus a small set of shared fields are
  enough. Do not build an `ExporterPlugin`/`MetricsRegistry` abstraction for one consumer.
- **Structured first, formatted second**: prefer fields over string interpolation so the same event
  is greppable, aggregatable, and machine-parseable.

---

## Log Levels

| Level | Use for | Gateway examples |
|---|---|---|
| `error!` | The system is failing in a way an operator must act on | config reload failed and LKG is now stale, upstream TLS verification failure, panic-adjacent invariant breach |
| `warn!` | Degraded but serving; retryable or expected-then-recovered | watch reconnect, config compile rejected, rate-limit tripped for a tenant, single upstream attempt failed but retry succeeded |
| `info!` | Lifecycle milestones, once per meaningful event (not per request) | startup with role/features, config snapshot published with generation, graceful drain started, shutdown complete |
| `debug!` | Per-request diagnostics, off in production | route matched, plugin chain executed, upstream selected, body size limits applied |
| `trace!` | Byte/chunk-level detail; almost always off | SSE chunk boundaries, JSON field extraction, retry budget accounting |

Rules:

- **Never `info!` on the per-request path** — at gateway volume that is a self-inflicted DoS on the
  logging pipeline. Per-request records are `debug!` at most.
- Plugin log lines carry `plugin = <instance name>` and remain separate from the data plane's single
  request record; `proxy/record.rs` emits that record once, with `authn` set to the authenticating
  instance name when one exists.
- At publication, exactly one `info!` line per plugin instance is emitted with fields `plugin`,
  `kind`, `scope` (`global` / `route` / `global+route` / `unused`), `phase`, `phases`,
  `buffers_body`, `enabled`. It is a lifecycle event, not a per-request event. Per-request `info!` is
  forbidden: a plugin's `LogLevel::Info` on the request path is emitted at `debug!` by the data
  plane; `info!` is reserved for lifecycle milestones.
- An expected client disconnect or cancellation is `debug!`, not `error!`. Report it with the
  log-only `http.status = 499` plus `error.category = Cancelled`; `499` is never sent on the wire.
- If you cannot say which action an operator takes after reading an `error!` line, it should be
  `warn!`.
- Do not log the same failure twice (once in a helper, once at the caller). Log at the boundary that
  can act.

---

## Structured Logging

### Field naming

- `snake_case` fields, dotted prefixes for grouping: `http.method`, `http.route`,
  `upstream.name`, `llm.protocol`, `llm.provider`, `llm.model`, `config.generation`.
- `authn` is the **instance** name, not the plugin kind: a route may run two `jwt` entries, and the
  record has to say which one claimed the request. Its value can never come from the request — it is
  written by the plugin that verified the credential, from the typed `Identity` slot in the request
  context — so a caller cannot put a string of its own into the field.
- Reserved field names, always the same spelling across crates:

| Field | Type | Meaning |
|---|---|---|
| `trace_id` | string | Correlates logs across plugins, provider calls, retries |
| `request_id` | string | Per-request identifier surfaced to the caller |
| `tenant` | string | Tenant / project isolation key; empty until a tenant model exists |
| `authn` | string | The plugin instance that authenticated the request (the `name` the document gave it); absent when nothing did |
| `plugin` | string | The plugin instance a failure/refusal is attributed to; absent when no plugin was involved |
| `route_id` | string | Compiled route identity |
| `upstream` | string | Selected peer / provider name |
| `config.generation` | u64 | Snapshot generation in use |
| `config.revision` | string | Source-scoped revision (etcd `mod_revision`, etc.) |
| `error` | `%` display | The error, via `error = %err` |
| `error.category` | `?` debug | `nico_core::ErrorCategory` |
| `elapsed_ms` / `duration_ms` | u64 | Timings; never log raw `Instant` |
| `http.method` | string | Request method |
| `http.status` | u16 | Status written to the caller; `499` for a cancelled request nobody answered, `0` when nothing was written |
| `body_bytes` | u64 | Request body bytes actually metered through the proxy |
| `llm.protocol` | string | Client protocol an AI route serves, e.g. `openai.chat`; absent on every other route |
| `llm.provider` | string | Provider kind an AI route speaks to, e.g. `openai`; absent on every other route |
| `llm.model` | string | `logical -> physical` as the front-end reported it; absent until the client body was decoded |
| `llm.stream` | bool | Whether the client asked for an event stream; present exactly when `llm.model` is |
| `llm.mode` | string | `convert` or `passthrough`, as the route itself reports it |
| `llm.model_map` | bool | Whether the route renames models; never the map's contents |
| `llm.routes` | u64 | Provider routes the published generation covers |
| `llm.capabilities` | string | The capability axes one side declares, comma-joined (`streaming,tools,…`), or `none` |
| `llm.version` | string | The wire revision the side is pinned to; **absent** when the vendor has no versioned spelling |
| `error.stage` | string | Which publication stage refused a document: `document`, `plugin` or `llm` |

### LLM request fields

An AI route (`routes[].provider`) adds four fields to the one request record. The data plane reads
them from the request context, never from a vendor:

- `llm.protocol` and `llm.provider` are **publication facts**: the codec and adapter the plan paired
  for the route. They exist for every request of the route, including one refused before its body was
  read.
- `llm.model` and `llm.stream` are **facts of the decoded body**: the model pair
  (`logical -> physical`) and whether the caller asked for a stream. `llm.model` is written with `?`
  (Debug), not `%`: its logical half is the caller's string, and Debug escapes a control character
  instead of letting it shape the record. A `passthrough` route never
  decodes a body and a body refused before encoding never reports them; both fields are absent rather
  than empty/false. `false` would claim the caller asked for a complete answer when nothing was
  decoded.
- A route that is not an AI route emits none of the four. Do not write an empty value to keep a
  column shape: the absence is the fact.
- An AI route's request record never contains the prompt, the response body, a header value,
  `Authorization` or a provider credential. The LLM seam's values (`LlmRequest`, `LlmPrepared`,
  `LlmExchange`, `Endpoint`, `WireRequest`, the prepared exchange) have hand-written `Debug`
  implementations that print names and lengths only.
- Assembly adds one `info!` line per registered codec and per registered adapter: the registry key,
  `llm.capabilities`, and `llm.version` when that side answers with one. It is the only place an
  operator can read what this build serves before a request runs, and the revision is a field and never
  a placeholder — a protocol with no versioned spelling reports none.
- Publication adds one `info!` line per AI route: `route_id`, `llm.protocol`, `llm.provider`,
  `llm.mode` and `llm.model_map` (whether the route renames models, never the map's contents). It is
  a lifecycle event, not a per-request one, and it is emitted **after** the snapshot store: a line
  written while compiling would claim a publication that a later stage of the same document can still
  refuse, and a retried document would repeat it.
- A publication stage that refuses a document logs once, with `error.stage` naming the stage, the
  revision, and the error's `Display`. The three stages share one line so a fourth cannot forget to
  report the revision or to mark the generation degraded.

### Event shape

```rust
tracing::info!(
    config.generation = snapshot.generation(),
    config.revision = %version.revision,
    routes = snapshot.route_count(),
    "runtime snapshot published"
);

tracing::debug!(
    trace_id = %ctx.trace_id(),
    route_id = %route.id(),
    upstream = %peer.name(),
    body_bytes = body_len,
    "request routed"
);
```

- Keep `message` (the trailing string) short, stable, and in lowercase. Make it a noun phrase
  ("runtime snapshot published"), never a sentence with interpolated values — those belong in fields.
- Log the error once with `error = %err`; if the source chain matters, add a dedicated field for the
  root cause instead of `{:#}` formatting in the message.

### Spans

Create a span per request and per outbound dependency call, and keep them nested:

```rust
let request_span = tracing::info_span!(
    "request",
    trace_id = %meta.trace_id(),
    tenant = %meta.tenant(),
    http.method = %method,
);

// In an async fn: attach the span to the future.
async move { /* work */ }.instrument(request_span).await
```

- In async code use `.instrument(span)` on the future, **never** `span.enter()` — the `_guard`
  from `enter()` lives across `.await`, corrupting the span stack and leaking the span into other
  tasks. Same class of bug as holding a `MutexGuard` across `.await`. `enter()` is only correct in
  synchronous scope.
- One request span, one span per retry attempt, one span per provider call.
- `#[tracing::instrument(skip_all, fields(...))]` on service methods; `skip_all` is mandatory unless
  you have verified every argument is cheap, `Debug`, and secret-free.
- Do not create spans inside per-byte/per-chunk loops.

---

## What to Log

- Startup: binary version, active Cargo features, selected config source type, deployment role.
- Config lifecycle: load, publish (with generation and revision), rejection with error category,
  degraded state entered/left, watch reconnect attempts.
- Resource lifecycle: drain started, in-flight requests at shutdown, forced termination.
- Capacity signals: body-size rejections, parse timeouts, rate-limit trips, retry-budget exhaustion.
- Upstream health transitions: circuit opened/closed, provider failover with reason.
- Anything an on-call engineer would need to reconstruct the last 10 minutes without a debugger.

## What NOT to Log

- Secrets: JWTs, API keys, provider keys, TLS private material, etcd credentials. Log at most a
  short hash or the last 4 characters, and only when it genuinely aids debugging.
- Full request/response bodies, full config documents, or full upstream error payloads. Log sizes,
  digests, and field names instead.
- PII: prompt contents, user identifiers beyond the tenant key, IP addresses in production logs
  (log them in the access log path only if policy allows, and mark them for retention review).
- High-cardinality values as metric labels (raw paths, user ids, model names in per-model metrics
  beyond an approved allowlist).
- Anything inside a hot loop without a rate limit or `trace!` gating.

---

## Metrics

- Metrics are separate from logs: use `metrics` + an exporter configured in `nico-server`. The
  global recorder is installed once at startup (`OnceLock`/`LazyLock`), never re-initialized on
  reload.
- Naming: `gateway_<subsystem>_<noun>_<unit>`; counters end in `_total`, timings in `_seconds`
  (histogram), gauges in `_current` or descriptive (`_age_seconds`, `_open`).
- Label cardinality is a budget. Allowed labels: `source`, `result`, `route_id`, `provider`,
  `status_class` (`2xx`/`4xx`/`5xx`), `error_category`. Not allowed: raw paths, full URLs,
  per-user values.
- Current config observability is `ConfigStatus` on the snapshot handle plus the `tracing` fields
  `source`, `config.generation`, `config.revision`, `digest`, `size`, `error.category`. The
  `metrics` crate and an exporter arrive with the phase-1 data plane; until then these names are the
  contract to emit against.
- Config subsystem baseline: `config_load_total{source,result}`, `config_reload_total{result}`,
  `config_compile_seconds`, `config_watch_reconnects_total`, `config_snapshot_age_seconds`,
  `config_degraded`.
- LLM usage metrics must distinguish gateway-measured values from provider-reported token usage —
  never present an estimate as a bill.

---

## Common Mistakes

| Mistake | Fix |
|---|---|
| `println!("...")` / `dbg!` left in code | Use `tracing`; clippy denies both |
| `tracing::info!` per request | Move to `debug!` with structured fields |
| Logging `{:?}` of the whole request | Log sizes and identifiers only |
| `#[instrument]` without `skip_all` on a struct holding secrets | `skip_all` then list the safe fields |
| `span.enter()` guard held across `.await` | Corrupts the span stack and leaks the span into other tasks | `.instrument(span)` on the future |
| Logging error in the helper and again at the caller | Log once at the acting boundary; use `.inspect_err(...)` when wrapping |
| Building metric labels from the request path | Normalize to `route_id` |
| Formatting `Instant`/`SystemTime` into the log message | Emit `elapsed_ms`/`duration_ms` fields |
