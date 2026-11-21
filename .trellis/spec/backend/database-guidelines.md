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
- `etcd-client` (etcd v3 via tonic/prost) is pinned in `Cargo.lock` and lives in
  `nico-config-etcd` behind that crate's `client` feature, which `nico-server`'s `etcd` feature
  (default **off**) gates out of the product binary. It is never a default dependency.
- Before bumping it, verify by compilation: Watch RPC fields (`created`, `canceled`,
  `compact_revision`, `events`, `fragment`), Auth (the token exchange, not a header), TLS, and
  endpoint failover behavior.
- Run `cargo tree -d` and check for a second `tonic`/`hyper`/TLS stack conflicting with Pingora's.
  A duplicate heavy stack is a design problem, not a lockfile nuisance.
- Single key holds one complete document (e.g. `/gateway/prod/resources.yaml`). Multi-key
  consistency is a later milestone with its own acceptance criteria.
- `bootstrap.max_bytes` must stay below the server's `max-request-bytes` (etcd 3.5 default ≈ 1.5 MiB);
  phase-1 budget is ≤ 1 MiB. Compress the route document before raising any server limit.

---

## The etcd Source: Transport and Credentials

The etcd source is infrastructure, so everything about *how* it is reached is a startup fact of the
bootstrap file. A reload replaces the document; it never changes the transport, the identity or the
credential.

| Field | Rules |
|---|---|
| `endpoints` | One or more. `https://` and plaintext (`http://` or bare `host:port`) may not be mixed: one client serves one transport, so a mixed list has no meaning. Shape (whitespace, host, numeric non-zero port) is unchanged. |
| `tls` | Optional block. Presence *is* TLS. Requires `https://` endpoints and refuses plaintext ones; an `https://` endpoint without the block is refused naming its index. `ca_file` is required inside it. |
| `tls.ca_file` | The **complete** trust root, replacing any platform store. There is no platform fallback and no `tonic/tls-roots`: the trust root must be a fact of the configuration, not of the host. |
| `tls.cert_file` / `tls.key_file` | Optional and paired: one half alone is refused under `tls.key_file`. The pair must form one signing identity. |
| `tls.domain_name` | Optional. One name is verified for **every** endpoint of the client; per-endpoint certificate names are out of scope. Validated with `nico_core::validate_host`. |
| `auth.user` / `auth.password_ref` | Required together. `password_ref` is an `env:`/`file:` reference (`nico_core::is_secret_ref`); a literal password is a startup error, never accepted. |

Rules:

- **No secrets in a validated bootstrap.** `EtcdTls` carries paths; `EtcdAuth` carries the reference.
  Neither carries key material or the password, so neither can leak one through `Debug`, `Clone` or a
  log line.
- **Everything is read twice.** The bootstrap opens `ca_file`, `cert_file` and `key_file` to fail a
  typo at startup; `source_build` reads them again when it assembles the client. A file swapped in
  between is a named startup error, not a handshake failure on the first request. This is the same
  rule inbound TLS follows, for the same reason. Both reads apply the same ceiling
  (`MAX_TLS_MATERIAL_BYTES`, 1 MiB) through the read itself, so a mis-pointed path cannot load an
  arbitrary file into the process, and a file the bootstrap read cannot fail the second read on
  size alone.
- **The password is resolved once, late.** `password_ref` is checked for shape at bootstrap and read
  when the source is built (byte-for-byte for `file:`, so a trailing newline is part of it). Its
  value never enters the validated bootstrap, which is why the password is not in the config a
  publication carries. The value is bounded at 8 KiB wherever it came from: `file:` at its read,
  `env:` at the rule both values end at — an environment variable passes no file read, so the
  terminal rule is what bounds it.
- **Transport failures are startup failures.** With credentials configured, connecting performs the
  etcd token exchange, so a refused credential fails the process (`EtcdError::Auth`) instead of
  becoming a per-request `401`. An unreachable endpoint or a failed handshake is
  `EtcdError::Connect`. There is no degraded form that silently downgrades to cleartext or to
  anonymous access.
- **The protocol double models both.** `MockServer` serves TLS from PEM bytes a test hands over and
  can require a credential pair, minting a token through the `Authenticate` RPC and checking it as
  `authorization` metadata — the same exchange `etcd-client` performs. It takes bytes, never paths, so
  the crate never learns a test directory.

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
- A byte ceiling is enforced **by the read**, never by a preceding `stat`: `metadata` and `open` are
  two syscalls on a name, the name may be replaced between them, and it may address no regular file at
  all (a FIFO reports length zero, a device reports whatever it likes). `nico_config::read_bounded` is
  the one reader: it refuses a non-regular file, reads at most `limit + 1` bytes, and reports a size
  that is a lower bound. Every reader of a document or a key file goes through it.
- A validator and a consumer must share one definition of identity. `routes[].host` is compared with
  `nico_core::hosts_match` by both the duplicate check and the router, and deduplicated under
  `host_key`; two spellings of that comparison is how a document gets refused for a duplicate the
  router would serve as one route.
- Secrets appear only as **references** (`env:VAR`, `file:/path`), never as literal values in the
  document.
- A field the schema accepts must have a consumer in the same change. An operator who writes a control
  believes it is in force, so a carried-but-inert field is worse than an absent one: either read it
  where its effect belongs, or leave it out of the schema so `deny_unknown_fields` refuses the document.
  `providers[].model_allowlist` is the worked example in both directions — it is the *provider's* model
  names, checked on the resolved name while the exchange prepares a request (an alias from `model_map`
  that resolves onto a listed name is served), and it refuses the rest with `403 model_not_allowed`
  before anything is encoded.

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
