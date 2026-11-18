# Backend Development Guidelines

> Rust backend conventions for this project.

---

## Overview

The backend is a Rust Cargo Workspace: a small protocol-agnostic kernel (`nico-core`), one HTTP
data plane on Pingora (`nico-http`), one LLM data plane (`nico-llm`), a dependency-free member
selection crate (`nico-lb`), config sources (`nico-config*`), built-in policies (`nico-plugins`),
and a single binary (`nico`, built by `nico-server`) that wires them together.

These specs encode **Rust best practices** applied to that architecture: ownership over cloning,
typed errors over `anyhow`, explicit bounds and deadlines, snapshots over locks, and a toolchain
that enforces what review would otherwise have to.

> **Status**: the workspace has a green baseline. `nico-core` owns the document model;
> `nico-http` carries routing, plugins, the upstream member pool and the byte-level LLM seam;
> `nico-llm` owns the canonical model, four client codecs and four provider adapters, the provider
> pool, the attempt chain and the token meter; `nico-lb` holds the selection contract both pools
> reuse; `nico-metrics` owns the self-reporting surface (recorder, rendering, readiness) without a
> socket or a framework; `nico-plugins` carries the built-in policies; `nico-server` wires it all
> and publishes one plan per generation. These files are the binding conventions for new code.
> Architecture rationale is in `.trellis/tasks/11-03-unified-gateway-analysis/design.md`;
> the empirically verified Pingora behaviour that constrains `nico-http` is in
> `.trellis/tasks/archive/2026-11/11-03-phase0-tech-verification/research/phase0-capability-report.md` §6.
---

## Pre-Development Checklist

Before writing backend code in a task:

- [ ] Read the relevant guide(s) below and the task's `prd.md` + `design.md`.
- [ ] Confirm the crate and module the code belongs to (see `directory-structure.md` dependency
      rules). If it does not fit an existing crate, that is a design question first.
- [ ] If the change touches an HTTP data plane or the Pingora integration, read the verified
      constraints in the phase-0 capability report §6 before designing hooks or flushing.
- [ ] Confirm the change does not create a dependency from `nico-core` to a protocol crate.
- [ ] Identify the error type this code returns and how it maps to `ErrorCategory`.
- [ ] Identify resource bounds (size, time, channel capacity) for anything touching I/O.
- [ ] Identify how the change behaves on cancellation, timeout, and reload.
- [ ] Confirm the feature(s) it must build under, and add the matrix case if new.
- [ ] No new abstraction without a second real consumer.
- [ ] For any `pub` item: `///` doc with `# Errors` / `# Panics` where applicable.
- [ ] For anything performance-sensitive: is there a profile or benchmark, or is it unjustified?

---

## Guidelines Index

| Guide | Description | Status |
|-------|-------------|--------|
| [Directory Structure](./directory-structure.md) | Workspace layout, crate/module boundaries, dependency direction, naming | Filled |
| [Persistence Guidelines](./database-guidelines.md) | Config store (etcd/local), LKG cache, revision semantics, schema validation | Filled |
| [Error Handling](./error-handling.md) | `thiserror` per layer, `ErrorCategory`, panic-free paths, retry legality, HTTP mapping | Filled |
| [Quality Guidelines](./quality-guidelines.md) | Toolchain, workspace lints, coding idioms, performance, forbidden/required patterns, testing, review checklist | Filled |
| [Logging Guidelines](./logging-guidelines.md) | `tracing` spans, log levels, reserved fields, metric naming and cardinality | Filled |
| [Pingora Guidelines](./pingora-guidelines.md) | Verified `0.9.1` hook/serve contracts, `ServerConf` mapping, data-plane error matrix | Filled |
| [Documentation](./quality-guidelines.md#documentation) | `//!` crate/module docs, `///` with `# Errors`/`# Panics`, `#[non_exhaustive]`, comment prefixes | Filled |
| [Performance](./quality-guidelines.md#performance-mindset) | Measure-first, `--release`, flamegraph, stack vs heap, `#[inline]` discipline | Filled |

---

## Quality Check

Run before reporting a task complete (details in `quality-guidelines.md`):

```bash
cargo +nightly fmt --all --check          # nightly: group_imports/imports_granularity are nightly-only
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features    # includes doc tests
cargo deny check
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

# The feature matrix, because `traffic` is a build that cannot serve an AI route at all
cargo check --workspace --no-default-features
cargo check -p nico-server --no-default-features --features traffic
cargo test -p nico-server --no-default-features --features traffic --test role_traffic
```

Plus the dependency-boundary and feature-matrix checks from `directory-structure.md`.

### Local gates: lefthook

The baseline is wired into versioned git hooks, orchestrated by [lefthook](https://lefthook.dev)
(`lefthook.yml`). The checks themselves stay in `scripts/gates/` because lefthook cannot compute
which cargo packages a staged file belongs to.

```bash
brew install lefthook        # or: go install github.com/evilmartians/lefthook@latest
lefthook install             # writes .git/hooks/{pre-commit,pre-push}
```

- `pre-commit` (fast, `scripts/gates/pre-commit.sh`): runs only when a `*.rs` / `*.toml` / `*.lock`
  path is staged. rustfmt check, `cargo check --workspace --all-targets` (every target's types,
  `tests/*.rs` included), **unit tests (`--lib`)** for the changed crates and their reverse
  dependencies, the 500-line file budget for staged files, and a warning when `src/` changed
  without a test change.
  It does **not** run the integration suite: those targets start real sockets, processes and file
  watchers, and they are the slowest thing in this repository. Each step prints its own duration,
  and a total over `SLOW_GATE_SECONDS` (default 120) warns — visible, not fatal, because turning
  slowness into a failure is how a gate gets bypassed.
- `pre-push` (full, `scripts/gates/pre-push.sh`): the **only** gate that runs the whole test suite.
  fmt, `clippy -D warnings`, `cargo test --workspace`, `cargo doc` with `RUSTDOCFLAGS="-D warnings"`,
  the feature matrix, and `cargo deny check` when installed.
- Manual invocation: `lefthook run pre-commit` / `lefthook run pre-push`.
- Build-script prerequisite: `protoc` must be on `PATH`. `etcd-client` and `nico-config-etcd`
  compile `.proto` files in their build scripts, and both gates build with `--all-features`, so
  every Rust change reaches them. Each script checks for it up front and fails with the install
  command, instead of letting a third-party build script panic. The container image needs neither:
  `etcd` is not in a role's default feature graph.
- Skips: `merge` and `rebase` for pre-commit (a replay would produce false failures).

Rules:

- A red hook is a real failure; fix it instead of bypassing. `SKIP_NICO_HOOKS=1` exists for tool
  outages and must be mentioned in the commit or PR; `git commit --no-verify` bypasses lefthook
  itself and is reserved for the same cases.
- Do not set `core.hooksPath` while lefthook manages hooks — it silently disables them.
- Hooks read the **working tree**, so stage everything the commit should contain first.
- Keep the fast path under a minute on an incremental build; a slow gate gets bypassed and stops
  protecting anything.

---

## Rust Rules of Thumb (quick reference)

| Topic | Rule |
|---|---|
| Ownership | Prefer `&T`/`&str`/`&[T]`; clone only when ownership transfer, `Arc` sharing, or an immutable snapshot is real |
| Copy | `Copy` only for small (≤~24B) plain-data types. `Bytes` is a cheap `Clone`, **not** `Copy` |
| Errors | `thiserror` in libraries, `anyhow` only in `nico-server::main`; no `unwrap`/`expect` outside tests |
| Dispatch | Generics/`impl Trait` for closed hot paths; `Arc<dyn Trait>` at config-compiled extension points; box at the boundary, not inside structs |
| Shared state | `ArcSwap<Snapshot>` read once per request; `OnceLock`/`LazyLock` for statics; no `Rc`/`RefCell`/`Cell` in async code |
| Async | Bounded channels, `CancellationToken`, whole-call deadlines, no guard held across `.await` |
| Data | `Bytes`/`BytesMut` for wire data; no `String` copies, no `serde_json::Value` on hot paths |
| Performance | Measure with `--release` first; no `#[inline]` or micro-optimization without a benchmark |
| Config | Strongly typed `serde` structs, `deny_unknown_fields`, validate then swap |
| Docs | `//!` per crate/module, `///` with `# Errors`/`# Panics` for public APIs; comments explain why (`// SAFETY:`, `// PERF:`, `// CONTEXT:`) |
| Tests | Behavior-named, one behavior per test, errors asserted too, doc examples are tests |
| Unsafe | Denied workspace-wide; a task of its own if ever required |

---

**Language**: All documents under .trellis/spec/ are written in English; task artifacts (prd/design/implement) follow their own language convention.
