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
| `debug!` | Per-request diagnostics, off in production | route matched, filter chain executed, upstream selected, body size limits applied |
| `trace!` | Byte/chunk-level detail; almost always off | SSE chunk boundaries, JSON field extraction, retry budget accounting |

Rules:

- **Never `info!` on the per-request path** — at gateway volume that is a self-inflicted DoS on the
  logging pipeline. Per-request records are `debug!` at most.
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
  `upstream.name`, `ai.model`, `config.generation`.
- `authn` is the **instance** name, not the plugin kind: a route may run two `jwt` entries, and the
  record has to say which one claimed the request. Its value can never come from the request — it is
  written by the filter that verified the credential, from the typed `Identity` slot in the request
  context — so a caller cannot put a string of its own into the field.
- Reserved field names, always the same spelling across crates:

| Field | Type | Meaning |
|---|---|---|
| `trace_id` | string | Correlates logs across filters, provider calls, retries |
| `request_id` | string | Per-request identifier surfaced to the caller |
| `tenant` | string | Tenant / project isolation key; empty until a tenant model exists |
| `authn` | string | The plugin instance that authenticated the request (the `name` the document gave it); absent when nothing did |
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
- AI usage metrics must distinguish gateway-measured values from provider-reported token usage —
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
