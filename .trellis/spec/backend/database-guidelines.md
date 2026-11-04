# Persistence Guidelines

> The gateway has no application database. This file covers its persistent state: the config
> store (etcd / local file), the last-known-good cache, and any future durable state.
> The filename follows the Trellis template; read it as "persistence and state storage".

---

## Overview

Three distinct storage roles, never conflated:

| Role | Implementation | Authority |
|---|---|---|
| **Config store** | local file (phase 1); etcd and Nacos are later milestones behind `ConfigSource` | Single authoritative source per process |
| **Last-known-good cache** | Local file (`cache_path`) | Fallback only, never a source of truth |
| **Runtime state** | In-memory `ArcSwap<RuntimeSnapshot>` + `SharedServices` | Derived; rebuilt from the config store |

Rules:

- One authoritative source per process, chosen by local bootstrap config. No implicit multi-source
  merging; a composite source must be an explicit implementation with declared precedence.
- The LKG cache is **not** a peer source. It may only be used to start when
  `allow_bootstrap_cache: true`, and must be marked `stale`.
- Nothing on the request path reads the store. Requests read the published snapshot.
- No client library is allowed to leak into `nico-core`; etcd/Nacos clients live in their own
  feature-gated crates.

---

## Client and Version Pinning

- The phase-1 source is the local file source (`nico-config::local`), already implemented and
  covered by tests for atomic rename, symlink swap, half-write, deletion/recreation and SIGHUP.
  The LKG cache is written after every successful publish and read only when
  `allow_bootstrap_cache: true`.
- `etcd-client` (etcd v3 via tonic/prost) is the **etcd milestone** candidate, not a phase-1
  dependency. The exact version is pinned in `Cargo.lock` only when that milestone starts; the
  re-verification checklist lives in the phase-0 capability report §8.
- Before adopting a version, verify by compilation: Watch RPC fields (`created`, `canceled`,
  `compact_revision`, `events`, `fragment`), Auth, TLS, and endpoint failover behavior.
- Run `cargo tree -d` and check for a second `tonic`/`hyper`/TLS stack conflicting with Pingora's.
  A duplicate heavy stack is a design problem, not a lockfile nuisance.
- Single key holds one complete document (e.g. `/gateway/prod/resources.yaml`). Multi-key
  consistency is a later milestone with its own acceptance criteria.
- `bootstrap.max_bytes` must stay below the server's `max-request-bytes` (etcd 3.5 default ≈ 1.5 MiB);
  phase-1 budget is ≤ 1 MiB. Compress the route document before raising any server limit.

---

## Query and Access Patterns

The only allowed interactions with the store:

1. **Load** — read the whole document once, compute a content digest, return a `ConfigVersion`
   (`revision`, `digest`, bounded `Bytes`).
2. **Watch** — subscribe from a revision, emit only `Snapshot(ConfigVersion)` or
   `ResyncRequired`. No incremental patches.
3. **Resync** — on compaction/gap/disconnect: full `load`, then resume watching.

Never: field-level reads, per-route keys, partial writes from the gateway, or store transactions.
The gateway is a read-only consumer of its own config.

### Revision semantics

- `Revision` is **source-scoped and opaque**. An etcd `mod_revision` must never be compared with a
  Nacos version or an MD5.
- etcd `start_revision` is inclusive: after `load` returning revision `R`, watch from `R + 1`.
- If a source can only prove equality (MD5), it must guarantee no duplication/no regression within a
  single watch connection; the server only deduplicates by digest and assigns a monotonic local
  generation. Never sort by an MD5.
- When ordering cannot be proven, prefer a full reload over guessing.

### Transactions and atomicity

- Publication atomicity comes from `ArcSwap`, not from store transactions.
- A reload builds a complete candidate snapshot off the request path, validates it, then swaps once.
  Failure keeps the previous snapshot and marks `config_degraded`.
- Never publish a partially read multi-key document. If required keys are unreadable or incomplete,
  emit `ResyncRequired` / keep LKG.
- A deleted key means "config unavailable", not "empty config": not-ready at startup, LKG + alert at
  runtime.

---

## Schema and Validation

- Documents are versioned YAML with a schema version field. Unknown fields are **rejected**, not
  silently ignored — silent ignoring makes a config look applied when it is not.
- `serde` with `#[serde(deny_unknown_fields)]` on config structs; each plugin owns a strongly typed
  `Config` type. `deny_unknown_fields` and `#[serde(default)]` do not compose well on the same
  struct — keep the strict doc-level structs free of defaults, and put defaults in the code that
  consumes the config, so a typo cannot be silently absorbed.
- Config structs derive `Debug` and are passed by reference (`&Config`); they are **not** `Copy`
  (they contain `String`/`Vec`). Only the tiny metadata (`generation: u64`, `Revision`) crosses
  boundaries by value.
- Validation stages, in order, all before publishing: parse → reference validation (routes point at
  existing upstreams/providers/filters) → semantic validation (limits sane, no duplicate ids) →
  construction succeeds.
- Bounds are enforced during parse: document size, nesting depth, route count, upstream count,
  compile time. Config is an untrusted input boundary.
- Secrets appear only as **references** (`env:VAR`, `file:/path`), never as literal values in the
  document.

---

## Naming Conventions

| Thing | Rule | Example |
|---|---|---|
| etcd key | `/<service>/<env>/<artifact>` | `/gateway/prod/resources.yaml` |
| Schema version field | `apiVersion` / `version` | `apiVersion: gateway/v1` |
| Route id | stable, human-readable, unique | `api-public-v1` |
| Upstream/provider name | lowercase kebab-case | `openai-primary` |
| Cache file | `<cache_path>/last-good.yaml`, atomic rename | `/var/lib/gateway/last-good.yaml` |
| Local file write | write temp + `fsync` + atomic `rename` | never in-place truncate |

Cache files carry metadata (revision, digest, generation, timestamp) alongside the document so
stale-ness is provable at startup.

---

## Common Mistakes

| Mistake | Why it hurts | Do instead |
|---|---|---|
| Treating LKG as a valid source | Silently serves old routes with no signal | Only with `allow_bootstrap_cache`, marked `stale`, alerted |
| Comparing revisions across sources | Wrong ordering, publishes stale config | Source-scoped opaque `Revision` |
| `start_revision = R` (inclusive) after load | Re-applies the same revision | Watch from `R + 1` |
| Publishing on each watch fragment | Half a document becomes live config | Aggregate to `fragment = false` or force resync |
| Incremental patches on the store | Transiently invalid intermediate config | Whole-document `Snapshot` only |
| Ignoring unknown fields | Typos look like working config | `deny_unknown_fields` |
| Writing the cache in place | Crash mid-write loses LKG | temp + fsync + atomic rename |
| Raising etcd `max-request-bytes` to fit a bloated document | Cluster-wide blast radius | Compress routes / design bounded multi-key |
| Logging the full document or secret | Credential leak | Log `source`, `revision`, `digest`, size |
