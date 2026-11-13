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
| `llm.protocol` | string | Client protocol an AI route serves: `openai.chat`, `openai.responses`, `anthropic.messages` or `gemini.generate_content`; absent on every other route |
| `llm.provider` | string | Provider kind an AI route speaks to: `openai`, `openai_responses`, `anthropic` or `gemini`; absent on every other route |
| `llm.model` | string | `logical -> physical` as the front-end reported it; absent until the client body was decoded |
| `llm.stream` | bool | Whether the client asked for an event stream; present exactly when `llm.model` is |
| `llm.mode` | string | `convert` or `passthrough`, as the route itself reports it |
| `llm.model_map` | bool | Whether the route renames models; never the map's contents |
| `llm.routes` | u64 | Provider routes the published generation covers |
| `llm.capabilities` | string | The capability axes one side declares, comma-joined (`streaming,tools,…`), or `none` |
| `llm.version` | string | The wire revision the side is pinned to; **absent** when the vendor has no versioned spelling |
| `llm.target` | string | The provider name an attempt was addressed to; the same spelling as `llm.provider`, so one dashboard can filter both |
| `llm.attempt` | u32 | 1-based number of the attempt in a route's sequence, across its targets |
| `llm.outcome` | string | `failed` or `skipped` — what became of an attempt the request did not end on |
| `llm.circuit` | string | `closed`, `open` or `half_open` — the phase a target's breaker moved to |
| `llm.excluded` | string | `rate_limited` or `credential` — why a pool member left distribution |
| `llm.tokens.kind` | string | One `TokenKind` name: `input`, `output`, `cache_read`, `cache_write` or `reasoning` |
| `llm.tokens.count` | u64 | What the provider reported for that kind. One line per reported kind; **no line for a kind it did not report** |
| `error.stage` | string | Which publication stage refused a document: `document`, `plugin` or `llm` |

### Token usage

`llm.tokens.*` carries the **provider's own numbers**, the ones a bill is made of. The gateway never
counts tokens itself: it has no tokenizer, and a self-counted number would never agree with the bill.

Two rules keep the field honest:

- **Reported is not measured.** These lines are not a gateway observation; the byte counts elsewhere
  in the record are. Do not mix them in one query.
- **Absent is not zero.** A provider that reported nothing produces no lines and no metric samples. A
  consumer that wrote zeros for those would pollute every average downstream.

The `model` label on the metric is the **physical** name the provider was asked for, taken from the
route's `model_map` value. When a provider declares no `model_map`, the caller's own model name becomes
the label — that is the one place cardinality can follow traffic, and the README says so.

### LLM request fields

An AI route (`routes[].provider`) adds four fields to the one request record. The data plane reads
them from the request context, never from a vendor:

- `llm.protocol` and `llm.provider` are **publication facts**: the codec and adapter the plan paired
  for the route. They exist for every request of the route, including one refused before its body was
  read. Their spellings are the registry keys — four client protocols (`openai.chat`,
  `openai.responses`, `anthropic.messages`, `gemini.generate_content`) and four provider kinds
  (`openai`, `openai_responses`, `anthropic`, `gemini`) — and `nico-llm`'s structure test pins both
  sets, because a log field a dashboard filters on is a contract, not an implementation detail. The
  second OpenAI dialect is a second *kind* (`openai_responses`), so `llm.provider` names one wire
  document shape per value.
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
  a placeholder — a protocol with no versioned spelling reports none. The revisions this build can
  report are `2023-06-01` (both Anthropic sides) and `v1beta` (both Gemini sides); both OpenAI
  dialects have no versioned spelling and leave the field out.
- Publication adds one `info!` line per AI route: `route_id`, `llm.protocol`, `llm.provider`,
  `llm.mode` and `llm.model_map` (whether the route renames models, never the map's contents). It is
  a lifecycle event, not a per-request one, and it is emitted **after** the snapshot store: a line
  written while compiling would claim a publication that a later stage of the same document can still
  refuse, and a retried document would repeat it.
- A publication stage that refuses a document logs once, with `error.stage` naming the stage, the
  revision, and the error's `Display`. The three stages share one line so a fourth cannot forget to
  report the revision or to mark the generation degraded.

### Attempt and circuit fields

A converted route that declared attempts reports them on three kinds of line, all about a target and
neither about a prompt:

- **One `warn!` per failed attempt the walk replaced**, written where the walk said "retry":
  `llm.target` (the provider name), `llm.attempt` (the 1-based number of the attempt that failed),
  `llm.outcome = "failed"` and `duration_ms` (the round's duration). Without it a retry that succeeds
  leaves the first failure invisible — the client only ever sees the final answer, so this line is the
  only record the attempt happened. The attempt that *produced* the client's answer takes no line: its outcome is
  the request record's, and a successful first attempt stays silent, as a per-request log must.
- **One `info!` per skipped target**, written by the round that skipped it: `llm.target` (the
  provider name), `llm.attempt` (the 1-based number of the attempt that follows — or would have,
  when the walk refused every target) and `llm.outcome = "skipped"`. A target whose breaker is open
  is never contacted, so this line is the only record that the route has one — without it an
  operator would read a silent gap in a chain.
- **One `warn!` per breaker phase change**, written where the change happens: `llm.target` and
  `llm.circuit` (`closed` / `open` / `half_open`). A phase change is an operator-visible fact — the
  target changed what it will admit — and it is the signal an alert fires on.

No line here carries a base URL, a header value or a credential: a target is named by its document
name, and the identity's `Debug` (which the circuit line goes through) omits the endpoint.
`llm.target` is the same field a request record's `llm.provider` uses, so one dashboard can filter
both by target name.

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

- Metrics are separate from logs: use `metrics` at the call site and one exporter, owned by
  `nico-metrics` and installed by `nico-server`. The global recorder is installed once at startup
  (`OnceLock`), never re-initialized on reload.
- **Where the code lives.** `nico-metrics` is the capability layer: recorder installation, Prometheus
  rendering, readiness projection, status line and content type. It depends on `nico-core`, `metrics`,
  `metrics-exporter-prometheus`, `serde`, `serde_json` and `thiserror` — and on **nothing else**. No
  pingora, no tokio, no `nico-http`: the exporter must stay out of the data plane's dependency tree,
  and the capability layer must be testable without a socket. The listener, the `ServeHttp` adapter
  and the port preflight live in `nico-server/src/metrics/`.
- **Where recording happens.** Each metric name has exactly **one** emission point, in the layer that
  owns the fact: configuration lifecycle facts in `nico-server` (`reload.rs`, `reload/publish.rs`),
  request facts in `nico-http` (`proxy/record.rs`), and the two scrape-time gauges in `nico-metrics`.
  Do not collect the names into a shared constant table: the table would have no second consumer and
  would make three crates depend on a fourth for nothing. This section is the contract.
- Naming: `gateway_<subsystem>_<noun>_<unit>` for new families; counters end in `_total`, timings in
  `_seconds` (histogram), gauges in `_current` or descriptive (`_age_seconds`, `_open`). The two
  families listed below are the exception and keep their existing names verbatim, because they were
  published as the contract before the template was.
- Label cardinality is a budget. Allowed labels: `source`, `result`, `route_id`, `provider`,
  `status_class` (`1xx`/`2xx`/`3xx`/`4xx`/`5xx`/`none`), `error_category`. Not allowed: raw paths,
  full URLs, per-user values, model names, credentials.
- `error_category` takes its values from `ErrorCategory::as_str()`, which is the same string as the
  enum's serde spelling, plus `none` for a request with no error. The set is therefore `client`,
  `upstream`, `limit`, `internal`, `cancelled`, `none`. `as_str` is exhaustive with no fallback, so a
  new variant is a compile error until it is given a spelling and added to this list — there is no
  catch-all value a new category could ship as.
- **`none` is a value, not an absence.** A request with no error category and one labelled
  `error_category=""` are different facts, and the empty string pollutes a group-by. Use the literal
  `none`. The same applies to `route_id`: a request that matched no route is labelled `unmatched`,
  never the empty string.

### The surface

A process can expose a read-only self-reporting listener on its own port. It is **off by default**:
with no `metrics` section in the bootstrap there is no second listener and no recorder installed, so
an unconfigured process behaves exactly as it did before the surface existed.

```yaml
metrics:                # optional; omitting the section keeps the surface off
  listen: 127.0.0.1:9090
```

| Endpoint | Answer |
|---|---|
| `GET /health/live` | `200 ok`, plain text. Reads **no** shared state: a process that can answer is alive, and a configuration failure must not turn a healthy process into a restart loop. |
| `GET /health/ready` | `200` when `HttpStateHandle::servable()`, else `503`. Body is a JSON object carrying `ready`, `generation`, `revision`, `digest`, `stale`, `degraded`, `last_error_category`, `last_reload_unix_secs`. |
| `GET /metrics` | Prometheus text (`text/plain; version=0.0.4`). |

A non-`GET` method on a known path answers `405` with `Allow: GET`; an unknown path answers `404`.
Neither reads shared state.

**Readiness is the request path's own predicate.** It is `HttpStateHandle::servable()`, the same
value the data plane consults before answering `503 not_ready`. It is **not** `ConfigStatus.ready`:
that flag is `true` on a last-known-good start while `degraded` and `stale` are also true, so using it
would report "cannot serve" for a process that is serving.

**`degraded` / `stale` do not make readiness fail.** Serving a last-known-good generation is designed
behaviour, and pulling every replica out of the endpoint list because the configuration source died
turns a partial failure into a total one. Both flags are reported in the body and as
`config_degraded`, which is what a deployment that wants "not fresh means no traffic" should alert on
or route on.

### The families

| Family | Type | Labels | Emission point |
|---|---|---|---|
| `config_load_total` | counter | `source`, `result` | one source read (`reload.rs`) |
| `config_reload_total` | counter | `result` | one publication attempt (`reload/publish.rs`) |
| `config_compile_seconds` | histogram | — | the two compile stages only: plugin compile and LLM compile, i.e. turning a parsed document into a servable plan. Parse is outside it (a document that fails to parse never compiles, and `error.stage = "document"` already reports that failure), and so is the last-known-good cache flush — that is file I/O, and folding it in would send an operator optimizing this number to the wrong place |
| `config_watch_reconnects_total` | counter | `source` | a received `ConfigEvent::ResyncRequired` — the file source has no reconnect, and this is the same fact: watch continuity was lost and the document must be re-read whole |
| `config_snapshot_age_seconds` | gauge | — | scrape time; **skipped** when no successful publish has been recorded, because a missing value is honest and a zero-second age would claim a publish that never happened |
| `config_degraded` | gauge | — | scrape time |
| `gateway_http_requests_total` | counter | `route_id`, `status_class`, `error_category` | `record::emit`, once per request |
| `gateway_http_request_seconds` | histogram | `route_id` | the same point |
| `gateway_upstream_selected_total` | counter | `upstream`, `endpoint` | the one emission point of the traffic-layer selection (`upstream/pools.rs::selected`) |
| `gateway_upstream_ejected_total` | counter | `upstream`, `endpoint` | the ejection transition (`upstream/pools/member.rs::record`) |
| `gateway_llm_pool_selected_total` | counter | `route_id`, `provider` | a pooled member admitted for an attempt |
| `gateway_llm_pool_excluded_total` | counter | `route_id`, `provider`, `reason` | a pool member retired by `429`/`401`/`403` |
| `gateway_llm_tokens_total` | counter | `route_id`, `provider`, `model`, `kind` | `record::record_tokens`, one sample per **reported** kind; no sample for a kind the provider did not report |

`gateway_llm_tokens_total`'s `model` is the **physical** name (`model_map`'s value); a provider with no
`model_map` makes the caller's model name the label, which is the one family whose cardinality can
follow traffic rather than configuration.

`status_class` maps the log-only `499` (a cancelled request nobody answered) to `4xx`, and the
nothing-written `0` to `none`.

### Recorder upkeep is a scrape-time obligation

`metrics-exporter-prometheus`'s `install_recorder()` starts **no** background task, and a histogram's
samples reach the renderer only through `run_upkeep()`. Without it a histogram renders empty, so
`Metrics::render` runs `run_upkeep()` immediately before `render()`. Do not spawn a task for this and
do not add a runtime to `nico-metrics`: the data upkeep maintains is only read at scrape time, so
nobody scraping means nothing to maintain.

`idle_timeout` stays at its default (`None`), so a series is never pruned. That is acceptable because
the cardinality is bounded by the route names the operator has written into documents, not by request
volume — but it does mean a series lives for the process lifetime.

### Failure modes

- A `metrics.listen` that cannot be bound, or a recorder that cannot be installed, is a **startup
  failure** (non-zero exit with the address or the reason). A surface that exists but renders nothing
  is the worst answer an operator can be given.
- A `metrics` section without `listen`, an unparseable address, or one equal to `http.listen` is a
  startup failure naming `metrics.listen`.
- The assembly logs one line either way — the address when enabled, `metrics surface disabled` when
  not — so nobody has to guess why a scrape fails.
- Per-request access logging is deliberately absent: a probe every few seconds would be a self-
  inflicted flood. Only a failure is logged.
- LLM usage metrics count only **provider-reported** token buckets, never a gateway estimate: the
  gateway has no tokenizer, and a self-counted number would never agree with the bill. A kind the
  provider did not report produces no sample, because a zero is a different fact from no answer.
- Circuit-breaker and rate-limit-table metrics are **not** part of this surface yet: exposing them
  needs `SharedServices` lifted to the assembly point and a read-only enumeration on `BreakerTable`,
  which is its own increment.

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
