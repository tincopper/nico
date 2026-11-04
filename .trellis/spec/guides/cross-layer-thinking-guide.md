# Cross-Layer Thinking Guide

> **Purpose**: Think through data flow across crate boundaries before implementing.

---

## The Problem

**Most bugs happen at crate boundaries**, not inside a single crate.

Why: within a crate the compiler enforces your invariants. At a boundary the value is a `Bytes`,
a `serde_json::Value`, or a struct whose invariants the receiving crate cannot see. Nothing forces
the sender's assumptions onto the receiver.

Common cross-layer bugs in this project:

- A config field is parsed by one crate and silently dropped by the next.
- A `CompiledRoute` carries a limit that the proxy path never reads.
- A provider response is decoded three different ways in three call sites.
- A request reads two different `RuntimeSnapshot` generations and mixes them.
- A stream parser reports usage that does not match the bytes actually delivered.

> "Layer" here means **crate boundary or process boundary** — `nico-core` / `nico-http` /
> `nico-ai` / `nico-config` / `nico-plugins` / `nico-server`, plus the edges to the
> client, the upstream, and the config store. It is not the generic "API / service / database"
> stack.

---

## Before Implementing Cross-Layer Features

### Step 1: Map the Data Flow

Draw out how data moves. Write the actual type at every arrow — not a description of the type.

```text
ConfigSource (Bytes + Revision)
  → nico-server (deserialize, validate, compile)
  → RuntimeSnapshot (ArcSwap)
  → nico-http (CompiledRoute + HttpContext)
  → nico-plugins (HttpFilter chain)
  → nico-ai (Provider + ProviderStream)
  → client (response bytes / SSE events)
```

For each arrow, ask:

- What is the exact type crossing here?
- Who owns decoding, and who owns validation?
- What happens when this arrow fails or is cancelled?
- Does the receiving side need the source identifier (`generation`, `revision`, `digest`, `seq`) to
  stay coherent?

### Step 2: Identify Boundaries

| Boundary                              | Common Issues                                                                       |
| ------------------------------------- | ----------------------------------------------------------------------------------- |
| `ConfigSource` ↔ `nico-server`     | Revision vs digest semantics, out-of-order events, raw bytes vs parsed document      |
| `nico-server` ↔ `RuntimeSnapshot`  | Partially validated config, lost generation, two snapshots in one request            |
| `RuntimeSnapshot` ↔ `nico-http`    | `CompiledRoute` missing resource limits, unstable filter ordering                    |
| `nico-http` ↔ `nico-plugins`    | Filter wants typed HTTP state but receives untyped metadata                          |
| `nico-http` ↔ `nico-ai`         | AI state leaking into `nico-core`, or created for non-AI requests                 |
| `nico-ai` ↔ `Provider`             | Request semantics (model mapping) vs response semantics (stream, usage) conflated    |
| `ProviderStream` ↔ client             | Lost incremental events, writes after cancel, usage not matching delivered bytes     |
| request ↔ request (state)             | Shared mutable state between two in-flight streams                                   |

### Step 3: Define Contracts

For each boundary, write down:

- The exact input type and where it is decoded.
- The exact output type and who may read its fields.
- Which errors are possible, and which `ErrorCategory` they map to.
- What happens on cancel, timeout, and reload.

If you cannot name the owner of a boundary, that is a design question — stop and resolve it before
coding.

---

## Common Cross-Layer Mistakes

### Mistake 1: Implicit Format Assumptions

**Bad**: Assuming a config field is always present because the local YAML has it.

**Good**: Every config struct uses `deny_unknown_fields` plus explicit `Option`/default handling,
and validation happens once in `nico-server`.

### Mistake 2: Scattered Validation

**Bad**: `nico-http` re-checks limits that `nico-server` already validated during compilation.
Two owners for one rule means the second one goes stale after a tuning change.

**Good**: Validate once at the entry point, then encode the guarantee in the type so the second
layer *cannot* re-check it:

```rust
// A validated limit carries the proof in the type, not in a comment.
pub struct BodyLimit(NonZeroUsize);

impl BodyLimit {
    /// The only constructor; rejects zero and anything above the configured ceiling.
    pub fn try_new(bytes: usize, ceiling: usize) -> Result<Self, ConfigError> { /* ... */ }
    pub fn bytes(&self) -> usize { self.0.get() }
}
```

`NonZeroUsize` alone only proves "non-zero" — it cannot express "≤ ceiling". A newtype is what
lets the receiver drop the check without losing the invariant.

### Mistake 3: Leaky Abstractions

**Bad**: `nico-core` gains an HTTP route concept "because both planes might need it".

**Good**: `nico-core` stays protocol-agnostic. If only one data plane uses a concept, it belongs
to that data plane's crate.

### Mistake 4: Every Consumer Parses The Same Payload

**Bad**: Several call sites index into raw provider JSON:

```rust
let model = body["model"].as_str().unwrap_or_default();
let total = body["usage"]["total_tokens"].as_u64().unwrap_or(0);
```

This looks local, but each consumer owns a private version of the provider contract. The next
response shape change updates one call site and misses another.

**Good**: Decode once at the owning boundary, then expose typed access:

```rust
let parsed: OpenAiResponse = serde_json::from_slice(&body)?;
let model = parsed.model;
```

**Rule**: For config documents, provider payloads, SSE frames, and any `serde_json::Value`,
exactly one owner defines:

- the type definitions,
- the decoding from bytes/`Value`,
- the projections other code reads,
- the reducers that turn stream events into state.

Formatting code may format fields; it must not redefine the payload contract.

> The duplication-detection angle of this pattern lives in
> [code-reuse-thinking-guide.md](./code-reuse-thinking-guide.md#pattern-4-repeated-payload-decoding).

### Mistake 5: One Request, Two Snapshots

**Bad**: A handler loads the snapshot again mid-request to read another field.

**Good**: Load `Arc<RuntimeSnapshot>` once at request start and thread it through. The whole
request, including the response stream, uses one generation.

### Mistake 6: Borrowing a Value Across a Reload

**Bad**: A long-lived stream keeps a `&CompiledRoute` (or a guard into the snapshot) alive across an
`await`, so a reload either leaks the old generation or the code is forced into an unsafe swap.

**Good**: Clone the `Arc<RuntimeSnapshot>` (or the specific `Arc` handle) and keep that. The old
snapshot is released when the last in-flight request finishes — which is exactly the desired drain
semantics.

---

## Config Compilation Boundary

Configuration is the highest-risk boundary in this project: bytes become compiled structures that
the hot path trusts without re-checking.

### Checklist: After Adding Or Changing A Config Field

- [ ] Add the field to the typed `serde` struct with explicit default/optional semantics — never a
      bare `serde_json::Value`.
- [ ] Add validation in `nico-server`'s compile step, not in the data plane.
- [ ] Confirm the field is carried into `RuntimeSnapshot` and reachable from the `CompiledRoute` or
      shared handle that needs it.
- [ ] Search for every reader of the old shape: `rg "<field_name>" crates/`
- [ ] Decide reload behavior: does this field require rebuilding shared services, or does a new
      snapshot suffice?
- [ ] Check backwards compatibility across revisions — cluster instances converge to the same
      revision eventually, not atomically. Add before remove.

**Real-world risk**: A field added to the config struct but not threaded into `CompiledRoute`
produces a config that validates, publishes, and then silently does nothing. This is invisible
without an explicit "field → consumer" search.

---

## Config Event → Snapshot → Observability Boundary

The reload path has its own cross-layer contract: an event from the config store must end up as
exactly one published generation, one metric sample, and one audit record.

```text
ConfigSource event (Snapshot | ResyncRequired)
  → nico-server (debounce, single-flight, validate, compile)
  → RuntimeSnapshot publish (ArcSwap)
  → metrics + tracing (generation, revision, digest, result)
  → audit log
```

### Checklist: After Changing The Reload Path

- [ ] **One event → at most one publish.** Digest dedup and revision ordering happen before
      compile, not after.
- [ ] **Failures are observable.** A rejected candidate increments the reload-failure metric and
      records the error category — it must not be silent just because LKG keeps serving.
- [ ] **`config_degraded` reflects reality.** Entered on an outage or a rejected reload, cleared
      only on a successful publish — not on "we tried again".
- [ ] **Metric labels stay bounded.** `source`, `result`, `error_category` — never a revision,
      digest, or key name.
- [ ] **Generation is the join key.** Logs, metrics, and audit for one reload all carry the same
      `config.generation`; a request's logs carry the generation it actually used.
- [ ] **No double counting.** A reconnect that re-publishes an identical digest must not look like
      a successful new reload.
- [ ] **In-flight requests are untouched.** A failed reload does not disturb the generation they
      already hold.

**Real-world risk**: The publish path succeeds but the metric path is skipped, so a config that
failed to apply looks healthy until someone reads the routes. Observability here is part of the
contract, not a nice-to-have.

---

## Streaming / Incremental Parse Boundary

`ProviderStream` is a per-response-stream state machine and a cross-crate contract between the
upstream provider and the client.

### Checklist: After Changing Stream Parsing

- [ ] State is per stream. No shared mutable parser, buffer, or counter across two concurrent
      responses.
- [ ] Cancellation is honored: after the client disconnects, no further writes and no leaked task.
- [ ] Partial frames are buffered correctly — a chunk boundary may split any SSE event, line, UTF-8
      sequence, or JSON value.
- [ ] Usage/metrics are derived from what was actually delivered, not from what the upstream
      announced.
- [ ] Errors that occur mid-stream are mapped to the same `ErrorCategory` the non-streaming path
      uses.
- [ ] At least one regression feeds a deliberately split, multi-chunk stream.

---

## Checklist for Cross-Layer Features

Before implementation:

- [ ] Mapped the complete data flow with concrete types
- [ ] Identified every boundary crossing
- [ ] Named the owner of decoding and validation at each boundary
- [ ] Decided where each error category is produced
- [ ] Decided cancel/timeout/reload behavior at each boundary

After implementation:

- [ ] Tested with edge cases (empty, missing, oversized, malformed)
- [ ] Verified error handling at each boundary, not just the happy path
- [ ] Checked the data survives the round trip unchanged
- [ ] Checked consumers import shared decoders instead of decoding locally
- [ ] Checked derived state points back to the source identifier (`generation`, `revision`, `seq`)
      instead of inventing a second cursor
- [ ] Checked no crate gained a dependency that violates the direction rules in
      `../backend/directory-structure.md`

---

## When to Create Flow Documentation

Create a dedicated flow document (in the task's `research/` or `design.md`) when:

- The feature spans 3+ crates
- The data format changes between boundaries
- The feature has caused a bug before
- Multiple consumers need the same data

---

**Core Principle**: 30 minutes of thinking saves 3 hours of debugging.
