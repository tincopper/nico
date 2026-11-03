# Backend Development Guidelines

> Rust backend conventions for this project.

---

## Overview

The backend is a Rust Cargo Workspace: a small protocol-agnostic kernel (`gateway-core`), one HTTP
data plane on Pingora (`gateway-http`), one AI data plane (`gateway-ai`), config sources
(`gateway-config*`), built-in policies (`gateway-plugins`), and a single binary (`gateway-server`)
that wires them together.

These specs encode **Rust best practices** applied to that architecture: ownership over cloning,
typed errors over `anyhow`, explicit bounds and deadlines, snapshots over locks, and a toolchain
that enforces what review would otherwise have to.

> **Status**: no product code exists yet. These files define the target conventions that
> `trellis-implement` / `trellis-check` must follow once implementation starts. Architecture
> rationale is in `.trellis/tasks/11-03-unified-gateway-analysis/design.md`.

---

## Pre-Development Checklist

Before writing backend code in a task:

- [ ] Read the relevant guide(s) below and the task's `prd.md` + `design.md`.
- [ ] Confirm the crate and module the code belongs to (see `directory-structure.md` dependency
      rules). If it does not fit an existing crate, that is a design question first.
- [ ] Confirm the change does not create a dependency from `gateway-core` to a protocol crate.
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
```

Plus the dependency-boundary and feature-matrix checks from `directory-structure.md`.

---

## Rust Rules of Thumb (quick reference)

| Topic | Rule |
|---|---|
| Ownership | Prefer `&T`/`&str`/`&[T]`; clone only when ownership transfer, `Arc` sharing, or an immutable snapshot is real |
| Copy | `Copy` only for small (≤~24B) plain-data types. `Bytes` is a cheap `Clone`, **not** `Copy` |
| Errors | `thiserror` in libraries, `anyhow` only in `gateway-server::main`; no `unwrap`/`expect` outside tests |
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

**Language**: All documentation is written in **English**.
