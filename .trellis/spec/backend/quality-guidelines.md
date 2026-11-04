# Quality Guidelines

> Code quality, linting, performance, testing, and review standards for the Rust backend.
>
> Structure follows the Apollo Rust Best Practices Handbook: idioms (ch.1), lints (ch.2),
> performance (ch.3), errors (ch.4, see `error-handling.md`), testing (ch.5), dispatch (ch.6),
> type-state (ch.7), documentation (ch.8), pointers (ch.9).

---

## Overview

Quality is enforced by the toolchain, not by review prose. Every rule below that can be a lint is
a lint, so review can focus on design.

Baseline commands — all must pass before a task is reported done (they are also wired into the
versioned lefthook gates, see `index.md`):

```bash
cargo +nightly fmt --all --check          # nightly: group_imports/imports_granularity are nightly-only
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features    # includes doc tests
cargo deny check
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

The `RUSTDOCFLAGS` is mandatory: `broken_intra_doc_links` is only a warning by default, so
`cargo doc` alone will **not** fail on a broken `[`link`]`.

If the test suite moves to `cargo nextest`, add `cargo test --workspace --doc` — nextest does not
run doc tests.

Per-feature matrix (features must be additive and independent):

```bash
cargo check --no-default-features
cargo check --features ai
cargo check --features ai,builtin-plugins,config-etcd
cargo check --all-features
```

---

## Design Principles and File Budget

Structure is derived from invariants, not chosen from a pattern catalogue. Before adding a module,
a trait or a configuration knob, be able to state three things: which concrete coupling it removes,
who the **second** real consumer is, and what breaks if it is deleted. If any answer is missing, do
not add the abstraction.

### Reason from invariants first

- Write down the invariants and failure modes before the structure: who may mutate the value, what
  must happen when the dependency is down, which resource bound applies, where cancellation lands.
- Let the type system carry the invariant. An illegal state that only a comment forbids is a defect
  waiting to happen (`ResponseCommitted`, `ParseLimits`, private fields + constructors).
- Optimize for the reader who must change the code under time pressure: explicit data flow, few
  moving parts, one obvious place to look.

### High cohesion: one boundary per module

- A file or module owns **one** business boundary or **one** data structure, including that
  structure's validation and invariants.
- If describing the module needs "and" ("parses config **and** publishes snapshots"), split it.
- Export the minimum API that boundary needs. Everything else is private or `pub(crate)`.
- A module that re-exports unrelated types, or that keeps growing utility functions, is a sign the
  boundary was never named.

### Low coupling: dependencies point at owners

- Dependency direction is fixed by `directory-structure.md`; cycles are never allowed.
- Pass data, not handles: values, immutable snapshots and `Arc<Snapshot>` cross boundaries; mutable
  borrows, callbacks into the caller, and half-constructed structs do not.
- Communicate through the seams this spec already defines (a domain trait, an event enum, the
  published snapshot). Do not add a side channel because it is convenient at the call site.
- Each module's failure semantics must be describable without reading its caller: typed error,
  `ErrorCategory`, and the state the caller keeps on failure (last-known-good, degraded, not-ready).

### Extensibility at existing seams

- Extend with an `enum` when the set is closed, with a domain trait when it is open. Do not build a
  registry, factory or lifecycle manager until the second real implementation exists.
- Prefer configuration-driven variation over new code branches; features gate capability, never
  default behaviour.
- New extension points are added where the domain already owns the vocabulary (HTTP filter in
  `nico-http`, provider in `nico-ai`), not in a central "plugins" catch-all.

### File budget: 300–500 lines, hard cap 500

- Target **300–500** lines per `.rs` file; **500 is a hard cap**, including an inline
  `#[cfg(test)]` module. At ~400 lines, plan the split before adding more.
- Split along responsibility, in this order of preference:
  1. data model vs its parsing/validation (`config.rs` + `config/` for validators);
  2. pure decision logic vs I/O (policy computation vs filesystem/network effects);
  3. state machine/lifecycle vs side effects;
  4. tests by scenario (`tests/<scenario>.rs`).
- Exceeding the cap after a split means the boundary is wrong: re-cut by business capability or data
  structure, not by technical layer. `types.rs`, `utils.rs`, `helpers.rs` and `common.rs` are signs
  of a split that dodged the naming decision.
- Files already above the cap are refactor backlog: do **not** keep adding lines to them. When they
  must change, split first and keep the split commit separate from the behaviour change.
- Check (must print nothing for new work):

```bash
find crates -name '*.rs' | xargs wc -l | awk '$1 > 500 && $NF != "total" {print}'
```

A staged file that is already above the cap may not grow: the pre-commit gate compares the staged
line count against `HEAD` (for a pure rename, against the path it came from — moving a file does not
grow it), so the only passing change to an oversized file is one that shrinks it.
Encoding content onto fewer lines to keep the count flat (`\n` escapes inside a fixture literal,
semicolon-chained statements) is the behaviour the cap exists to stop — move the fixture to the file
that owns it and split, do not compress.

### Module layout for large logic

Prefer the directory form to keep one boundary per file while giving the reader a single entry
point: `reload.rs` declares `mod debounce; mod publish;` and `reload/{debounce,publish}.rs` hold the
submodules. The parent module keeps the public contract; children stay `pub(crate)` unless they own
their own public API.

---

## Toolchain and Workspace Configuration

`rust-toolchain.toml` pins the version; `Cargo.toml` sets `resolver = "2"` (`"3"` for edition 2024),
workspace dependencies, and workspace lints.

```toml
# Cargo.toml (shape, not verbatim)
[workspace]
resolver = "2"
members = ["crates/*"]

[workspace.package]
edition = "2021"
rust-version = "1.8x"        # pin the real MSRV; record why in the task
license = "..."              # required by cargo-deny

[workspace.dependencies]
tokio = { version = "1", default-features = false }
tokio-util = "0.7"           # CancellationToken
bytes = "1"
arc-swap = "1"               # RuntimeSnapshot publication
async-trait = "0.1"          # only if the pinned Rust cannot express async-in-trait for dyn
serde = { version = "1", features = ["derive"] }
thiserror = "2"
tracing = "0.1"
```

### Dependency policy entries name their reason

`deny.toml` is a gate, so an exception in it is a decision with evidence, not a mute:

- Every `[licenses] allow` and `[advisories] ignore` entry carries a comment naming the crate that
  needs it and the path that pulls it in (`brotli`/`zstd`/`serde_yaml` arrive as **non-optional**
  dependencies of `pingora-core`, which is why they cannot be compiled out).
- An `ignore` says why the advisory does not apply here (`unsafe-libyaml` is reachable only from
  `ServerConf`'s YAML helpers, which this build never calls).
- Re-check every entry when the pulling dependency is upgraded; delete the entry instead of
  widening it.

The YAML crate is **not** in `[workspace.dependencies]` above on purpose: upstream `serde_yaml` is
archived (no security fixes). The pinned choice is `serde_norway = "=0.9.42"` — a maintained fork
with `deny_unknown_fields` support, MSRV 1.71. Read `yaml::` in spec examples as "the pinned
crate", and never reach for `serde_yaml` just because it is the well-known name.

Workspace lint priorities: `nonstandard_style` and `future_incompatible` must carry
`priority = -1`. Without it `clippy::lint_groups_priority` fails the build under `-D warnings`
because `clippy::all` (priority 10) overlaps them.

Lint levels are configured at workspace scope (that is configuration, not an override); individual
sites are silenced only with `#[expect]` + `reason`:

```toml
[workspace.lints.rust]
unsafe_code = "deny"
unused_must_use = "deny"
unreachable_pub = "warn"
nonstandard_style = "deny"
future_incompatible = "warn"

[workspace.lints.rustdoc]
broken_intra_doc_links = "deny"
bare_urls = "deny"

[workspace.lints.clippy]
all = { level = "deny", priority = 10 }
# Performance and correctness lints worth calling out explicitly.
redundant_clone = { level = "deny", priority = 9 }
large_enum_variant = { level = "deny", priority = 9 }
clone_on_copy = { level = "deny", priority = 9 }
needless_borrow = { level = "deny", priority = 9 }
needless_collect = { level = "deny", priority = 9 }
manual_ok_or = { level = "deny", priority = 9 }
map_unwrap_or = { level = "deny", priority = 9 }
await_holding_lock = { level = "deny", priority = 9 }
# Panic-free policy (restriction lints are not in `all`, so they must be named).
unwrap_used = { level = "deny", priority = 8 }
expect_used = { level = "deny", priority = 8 }
panic = { level = "deny", priority = 8 }
unreachable = { level = "deny", priority = 8 }
todo = { level = "deny", priority = 8 }
unimplemented = { level = "deny", priority = 8 }
dbg_macro = { level = "deny", priority = 8 }
print_stdout = { level = "deny", priority = 8 }
print_stderr = { level = "deny", priority = 8 }
# Documentation lints, because public APIs are documented here.
missing_errors_doc = { level = "warn", priority = 7 }
missing_panics_doc = { level = "warn", priority = 7 }
missing_safety_doc = { level = "warn", priority = 7 }
empty_docs = { level = "deny", priority = 7 }
# Strict groups: allowed at bootstrap, flipped to "warn" once the tree is clean.
# Under `-D warnings` a "warn" here is effectively a hard failure, so enabling these on
# day one would force the team into mass `#[allow]`. See the rollout note below.
pedantic = { level = "allow", priority = 3 }
nursery = { level = "allow", priority = 3 }
```

`priority` decides which lint wins when two conflict (higher wins). Each crate opts in:

```toml
[lints]
workspace = true
```

Restriction-group lints (`unwrap_used`, `expect_used`, `panic`, `unreachable`, `todo`,
`unimplemented`, `dbg_macro`, `print_stdout`, `print_stderr`) are **not** part of `clippy::all`, so
they must be named explicitly — which is why they appear individually above.

`clippy.toml` carries the settings that cannot be expressed as a lint level. Two are mandatory,
otherwise the panic-free policy makes tests unwritable under `--all-targets`:

```toml
# clippy.toml
allow-unwrap-in-tests = true
allow-expect-in-tests = true
allow-panic-in-tests = true

# Pin the MSRV here too; clippy uses it to gate suggestions.
msrv = "1.8x"
```

Without these, `cargo clippy --all-targets` would reject `expect("fixture parses")` inside
`#[cfg(test)]`, which is exactly where the handbook says `expect` belongs. In tests, prefer
`expect("<what failed>")` over `unwrap()` so a failure names the fixture.

### `missing_docs` is per-crate, not workspace-wide

`missing_docs` is **not** in `[workspace.lints.rust]` above. Because the baseline runs
`-D warnings`, a workspace-level `warn` would be promoted to an error in the binary too and force
internal documentation of `nico-server` internals. Instead each library crate declares it at its
root:

```rust
// crates/nico-core/src/lib.rs
#![deny(missing_docs)]
```

The workspace still carries the rustdoc lints (`broken_intra_doc_links`, `bare_urls`) as `deny`,
so a bad link fails `RUSTDOCFLAGS="-D warnings" cargo doc` regardless of the crate.

### Rollout note for `pedantic` / `nursery`

`pedantic` and `nursery` are "rather strict or have occasional false positives". Because the
baseline runs `-D warnings`, setting them to `warn` makes them **hard errors** immediately.

- Bootstrap phase: they are `allow` in the workspace config above. CI gates on `clippy::all` and
the explicitly named lints.
- Once the tree is clean, flip `pedantic` to `warn`, fix the fallout in one focused task, then
consider `nursery`.
- Do **not** pre-empt the fallout with `#[allow]` — that is how a lint policy dies.

### Silencing a lint

Never `#[allow(...)]`, and never at module or crate scope. `#[expect]` warns when the lint stops
firing, which is what you want:

```rust
// Faster matching is preferred over size efficiency; boxing would add an allocation per match.
#[expect(clippy::large_enum_variant, reason = "variant is small enough in practice; see bench")]
enum Message {
    Code(u8),
    Content([u8; 1024]),
}
```

Same rule for the panic lints — a genuinely provable invariant is expressed as:

```rust
#[expect(clippy::expect_used, reason = "route table is non-empty by construction in RouteTable::try_new")]
fn first_route(&self) -> &CompiledRoute {
    self.routes.first().expect("non-empty by construction")
}
```

---

## Coding Idioms

Rust's ownership system prefers borrow over clone. These are the patterns review looks for.

### Ownership and Copy

- Prefer `&T`, `&str`, `&[T]` in parameters; `&str` over `&String`, `&[T]` over `&Vec<T>`.
- Pass by value when the type is `Copy` **and small** — up to 2–3 words (~24 bytes), plain data,
  no heap fields: `u64`, `Duration`, `bool`, `Option<NonZeroU32>`, small `#[derive(Copy)]` structs.
- `Bytes` is **not `Copy`** — it is a cheap `Clone` (pointer + refcount). Pass `Bytes` by value
  because cloning is cheap, not because it is `Copy`.
- Clone only when: ownership transfer is required by an API, the value is `Arc`/`Rc`, an immutable
  snapshot must be preserved, or the result is cached. Leave the clone to the last moment.
- Avoid clone traps: `map(|x| x.clone())` → `.cloned()` / `.copied()` at the end of the chain;
  cloning `Vec<T>`/`HashMap<K, V>`; cloning "because the borrow checker said so".
- Never clone a borrowed parameter to get ownership — take it by value and make the caller's
  intent explicit.
- `Cow<'_, T>` when ownership is genuinely conditional (`Cow<'_, str>` for a value that is
  usually borrowed).
- `Copy` derives only on types whose fields are all `Copy`. Arrays of `Copy` are stack-allocated —
  a large array can overflow the stack.
- When a small value type exposes **three or more** same-shaped fields, address them through one
  field enum instead of a getter and a setter per field: `SchemaBounds::get(Bound::Routes)` /
  `SchemaBounds::with(Bound::Routes, 1024)`, where `Bound::name()` is also the configuration key the
  field is written as. The names then live in one table, a new field is one variant plus one match
  arm, and the compiler points at every consumer that must decide something about it. Two fields, or
  fields that mean different things, keep named accessors (`Limits::max_request_body_bytes`,
  `Route::path_prefix`): an enum for them is ceremony, not cohesion.

### Option / Result

- `?` to propagate. If the `Err` value is not needed, do not `match` it.
- `let ... else { return ... }` when the failure path is expected and needs no value:
  `let Some(cfg) = self.cfg.get(name) else { return Err(...); };`
- `if let ... else` only when the else branch needs extra computation.
- `match` when you must inspect inner variants, or when transforming `Result<T, E>` into
  `Result<Option<T>, E>`.
- `.ok()`, `.ok_or(...)`, `.ok_or_else(...)` instead of hand-written `Result`↔`Option` matches.
- Avoid eager allocation: `ok_or_else`, `unwrap_or_else`, `map_or_else` when the fallback builds
  a `String` or collection; `unwrap_or_default` when the default is `Default`.
- `inspect_err` to log a failure while transforming it:
  `.inspect_err(|err| tracing::error!(error = %err, "compile failed")).map_err(CompileError::from)?`

### Iterators

- Iterators for transformation, composition, `enumerate`, `windows`, `chunks`, or combining
  sources without intermediate allocations.
- `for` loops for early exit (`break`/`continue`/`return`), simple side effects, or when a chain
  would hurt readability. Logging inside a chain uses `inspect`/`inspect_err`.
- `.iter()` over `.into_iter()` unless ownership is needed; `.iter()` for `Copy` element types.
- `.sum()` for sums, not `.fold()` — `sum` is specialized and the closure is opaque to the
  optimizer.
- Do not `collect()` just to hand the collection to another iterator. Pass
  `impl Iterator<Item = T>` instead.
- Iterators are lazy: nothing runs until a consumer is called, and chains fuse into one loop.

### Imports

Order: `std` → external crates → workspace crates → `super::` → `crate::`, blank line between
groups. Enforce it rather than eyeball it:

```toml
# rustfmt.toml — these are nightly-only options
reorder_imports = true
imports_granularity = "Crate"
group_imports = "StdExternalCrate"
```

Because of that, the formatting gate is `cargo +nightly fmt --all --check` when these options are
enabled; `cargo fmt` on stable silently ignores them.

Glob imports (`use foo::*`) are forbidden except in `#[cfg(test)] mod tests { use super::*; }`.

---

## Performance Mindset

**Don't guess, measure.** Rust is already fast; do not "optimize" without evidence.

- Benchmark and profile with `--release`. A "slow" Rust program is usually a debug build.
- `cargo clippy -- -D clippy::perf` for perf hints; `cargo bench` (criterion) for micro-benchmarks.
- Profile with `cargo flamegraph` (or `samply` on macOS). Box width = time or call frequency;
  color is random and meaningless. Thick stack = heavy CPU, thin stack = cheap.
- A benchmark improvement under ~5% is not worth the added complexity.
- `#[inline]` only when a benchmark proves it helps — rustc inlines well without hints.
- Stack vs heap: keep small `Copy` types on the stack; pass types larger than ~512 bytes by
  reference, not by value.
- Large buffers must not be built on the stack first:
  `let buffer: Box<[u8]> = vec![0; 65536].into_boxed_slice();` — **not**
  `Box::new([0u8; 65536])`, which materializes the array on the stack.
- Recursive data structures are boxed (`Box<...>`, `Vec<T>` is already boxed).
- For large const arrays consider `smallvec` (array-like API, heap for large sizes).
- Avoid intermediate collections; avoid cloning in loops; avoid `String` where `&str`/`Bytes` fit.
- Wire data uses `Bytes`/`BytesMut`; never round-trip through `String` on the request path.

---

## Forbidden Patterns

| Pattern | Why | Instead |
|---|---|---|
| `unwrap()` in non-test code | A panic in a worker kills in-flight traffic | `?` into a typed error |
| `expect()` outside a provable local invariant | Same | `?`, or `#[expect(clippy::expect_used, reason = "...")]` |
| `panic!` in request/config paths | Triggerable by input | Return a typed error |
| `todo!` / `unimplemented!` in merged code | Ships a guaranteed panic | Land the implementation, or `#[ignore = "reason"]` on the test |
| `unreachable!` in request/config paths | Same | Return an error. Allowed **only** where a type-state invariant makes it impossible, with `#[expect(clippy::unreachable, reason = "...")]` and a comment naming the invariant |
| `unsafe` | Not needed in this architecture | Safe abstractions; a dedicated task + `// SAFETY:` + Miri if ever required |
| `Box<dyn std::error::Error>` in a public API | Unmatchable, loses classification | `thiserror` enum |
| `anyhow` in a library's public signature | Same | `thiserror`; `anyhow` only in `nico-server::main` |
| `String` as the only error variant | No structure, no source chain | Typed variant with `#[source]` |
| Error types not `Send + Sync + 'static` | `?` across `.await` and in spawned tasks fails to compile | Derive/verify the bounds |
| `.clone()` to satisfy the borrow checker | Hides an ownership design problem | Restructure borrows; `Arc` when sharing is real |
| `String` for wire data | Extra allocation and copy per byte | `Bytes` / `BytesMut` / `&str` |
| `Rc` / `RefCell` / `Cell` in shared or async code | Not `Send`/`Sync`; `RefCell` panics at runtime | `Arc` + `OnceLock`/`ArcSwap`/`Mutex` |
| `tokio::sync::mpsc::unbounded_channel` | Unbounded memory growth under load | Bounded channel with explicit overflow behavior |
| `std::sync::Mutex` guard held across `.await` | Deadlock or `!Send` future | Snapshot + `ArcSwap`, or `tokio::sync::Mutex` |
| `tokio::spawn` without owner/cancellation handle | Orphan tasks survive reload and leak | Tie to `CancellationToken` and keep the `JoinHandle` |
| `spawn_blocking` for cheap CPU work | Thread-pool thrash | Inline it; reserve for genuinely blocking I/O |
| `#[allow(...)]` at module/crate scope | Accumulating, invisible debt | Fix the code, or `#[expect]` with `reason` |
| `serde_json::Value` on the request path | Untyped lookups and allocations | Strongly typed structs |
| Glob imports | Hides dependencies | Explicit imports (glob only in `mod tests`) |
| `pub` fields on public structs | No invariant protection, breaking change | Private fields + accessors |
| Feature flags that change unrelated behavior | Feature combinations become untestable | Features add capability; behavior is runtime config |
| `Box<dyn Trait>` inside a struct field by default | Premature type erasure, allocation, vtable | Generic field `Renderer<B: Backend>`; box at the API boundary |
| `dyn Trait` from a trait with generic methods | Not object-safe | Keep the trait object-safe; use generics instead |

---

## Required Patterns

- **Borrowing over cloning** (see Coding Idioms).
- **Typed errors per layer** (see `error-handling.md`).
- **Dispatch: static where you can, dynamic where you must.** Generics/`impl Trait` for
  hot-path internals and single-use implementations; `Arc<dyn Trait>` at config-compiled extension
  points and heterogeneous collections. `&dyn Trait` when ownership is not needed. Box at API
  boundaries, not internally.
- **Trait objects must be object-safe**: no generic methods, no `Self: Sized` requirement,
  `&self`/`&mut self`/`self` receivers only. Async methods need `async_trait` or a boxed future
  unless the pinned Rust expresses them for `dyn`.
- **Snapshots over locks** for shared runtime state: `ArcSwap<RuntimeSnapshot>`, loaded once per
  request and held for its whole lifetime.
- **Statics use `OnceLock`/`LazyLock`**, not `lazy_static`/`once_cell::sync::Lazy`; read-only
  shared slices use `Arc<[T]>`.
- **Explicit resource bounds**: body size, header count, parse depth, route count, channel capacity,
  concurrent reloads (single-flight).
- **Cancellation and deadlines in the signature**: `CancellationToken` plus a whole-call deadline
  from `RequestMeta`.
- **Idempotent reload**: build a complete candidate snapshot, validate, publish atomically;
  failure keeps last-known-good.
- **Type-state where illegal transitions are possible** (e.g. response committed vs not).
  `PhantomData` is zero-cost. Do **not** use it for trivial states, when runtime flexibility is
  needed, or when it forces over-complicated generics.
- **Documentation** (see Documentation section below).
- **Comments explain why, doc comments explain what/how.**

---

## Documentation

Rustdoc is first-class here; docs are part of the contract, and doc examples are tests.

| Level | Requirement |
|---|---|
| Crate (`lib.rs`) | `//!` explaining what the crate does, the problem it solves, and a pointer to its main modules |
| Module | `//!` explaining purpose, exports, and invariants |
| Public item | `///` explaining role, invariants, and usage |
| Fallible function | `# Errors` section listing when each `Err` is returned |
| Panicking function | `# Panics` section (there should be almost none) |
| Public `unsafe fn` | `# Safety` section (there should be none; `unsafe` is denied) |
| Public enum that may grow | `#[non_exhaustive]` |

- Examples in `///` blocks are **tested by `cargo test`**. Prefer them for public APIs; hide
  boilerplate lines with a leading `#`. `no_run` for side-effecting examples, `compile_fail` for
  demonstrating wrong usage.
- Doc examples must cover happy path plus at least one error path for fallible APIs.
- Code comments are reserved for: safety (`// SAFETY:`), performance (`// PERF:`), design context
  (`// CONTEXT:` with an ADR/design-doc link), platform quirks, and non-obvious assumptions.
  Prefix them so they are greppable.
- **Do not write living documentation in comments.** Comments rot silently. If something must
  outlive the PR, put it in `design.md`, an ADR, a doc comment, or a test.
- Restating the code (`// increment i by 1`) is a defect. If a function needs a long explanatory
  comment, split it into named functions instead.
- Every `TODO` carries an issue reference: `// TODO(#42): ...`.

---

## Testing Requirements

Tests are the first place a reader looks to understand behavior — they are living documentation.

### Layout and organization

- Unit tests: `#[cfg(test)] mod tests` in the file under test.
- Group tests per unit with a module so the test path reads as documentation:
  `mod test { mod compile { fn keeps_lkg_when_yaml_invalid() {...} } }` →
  `test::compile::keeps_lkg_when_yaml_invalid`.
- Integration tests: `crates/<crate>/tests/*.rs` — they only see the public API.
  **This is why `nico-server` ships `src/lib.rs` alongside `src/main.rs`**: the wiring must be
  reachable from an integration test.
- Doc tests: `///` examples on public items.
- Shared fixtures: `src/test_support.rs` behind `#[cfg(any(test, feature = "test-support"))]`.
- **One real server per integration-test binary.** A framework that owns the calling thread and
  installs a global runtime (`pingora::server::Server::run`) must be started once per test target;
  a second one in the same process tests the framework's globals, not our code. Scenario files in
  that target then assert sequentially against that one server, waiting on real events (a log line,
  a mock's observation, a received chunk) instead of sleeping. A unit test that needs a real server
  belongs in its own `tests/` target, not next to fast `#[test]` unit tests.
- Fixtures shared by several integration tests of one crate live in `crates/<crate>/tests/support/mod.rs`.
  Each `tests/*.rs` is its own crate, so it compiles only the helpers it calls and the rest is
  unreachable: that module carries `#![expect(dead_code, reason = "…")]`, and the exemption
  disappears by itself once a helper is deleted. Moving fixtures and harnesses there is how a scenario
  test file stays inside the 500-line cap — never by dropping assertions or compressing fixtures.
  A helper that only one test file calls stays in that file.

### Naming and assertions

- The name states the behavior and the condition:
  `config_compile_should_keep_last_known_good_when_yaml_is_invalid`.
- One behavior per test, ideally one assertion.
- Assertion messages state the actual vs expected state:
  `assert!(matches!(err, MyError::BadInput(_)), "expected BadInput, got {err:?}")`.
- For an `Ok` scenario, print the `Err` case in the message so a failure is diagnosable:
  `assert!(parse(input).is_ok(), "unexpected error: {:?}", parse(input).unwrap_err())`.
- `pretty_assertions` for readable diffs; `rstest` for parameterized cases **with named cases**
  (`#[case::single("a")]`) — named cases keep the test output self-describing.
- Error types derive `PartialEq` where practical so tests can `assert_eq!`; otherwise assert on
  `to_string()`. **Errors themselves must be tested**, not just the success path.
- `#[should_panic]` only when panicking is the intended contract; prefer `Result`.
- `#[ignore = "reason"]` for knowingly unfinished tests — never leave them silently passing.

### Async and time

- `#[tokio::test(flavor = "current_thread")]` unless concurrency is the subject.
- `tokio::time::pause()` + `advance` for debounce/timeout logic; never `sleep` in a test.
- Determinism: no wall clock, network, or filesystem ordering dependencies. Config sources are
  tested through `FakeConfigSource`.

### Coverage expectations by layer

| Layer | Must be covered |
|---|---|
| `nico-config` | digest dedup, out-of-order/regressed revisions, watch disconnect, compaction/gap → `ResyncRequired`, oversized document, invalid UTF-8/YAML, unknown fields rejected, compile failure keeps LKG, subsequent fix publishes, debounce + single-flight, in-flight requests unaffected during outage |
| `nico-http` | route matching, filter ordering, response commit state, retry legality, deadline propagation, SSE/WebSocket streaming boundaries, backpressure and slow client |
| `nico-ai` | SSE chunk-splitting across event/line/UTF-8/JSON boundaries, provider error mapping, usage accounting separation (measured vs reported), no fallback after first byte |
| `nico-core` | error category mapping, deadline/cancellation semantics |

### Property, snapshot, and integration testing

- `proptest` for parsers (SSE splitter, schema validation, body-size accounting). Invariant: any
  byte split of a valid stream yields the same event sequence.
- `insta` for structural output (compiled route tables, error responses). Rules:
  - **Named** snapshots: `assert_yaml_snapshot!("routes/compiled", table)`.
  - **Small** snapshots — snapshot the relevant subtree, never a whole snapshot object.
  - **Redact** unstable fields (timestamps, digests, generated ids):
    `".digest" => "[digest]", ".created_at" => "[timestamp]"`.
  - Do not snapshot primitives or small structs (`assert_eq!` is clearer); do not snapshot
    external resources.
  - Commit snapshots to git and read the diff before accepting.
- Real-dependency integration tests where a fake would not exercise the client: etcd
  watch/compaction/auth via `testcontainers` with a pinned image. A `FakeConfigSource` is not a
  substitute for the real client's stream semantics.
- Benchmarks (`criterion`) live in `benches/`, are not CI-gating, and run with `--release`.

---

## Code Review Checklist

- [ ] Does the change respect the dependency direction (`nico-core` untouched by protocol
      crates)? Run the `cargo tree` checks from `directory-structure.md`.
- [ ] Is any new abstraction backed by **two real consumers**? If not, inline it.
- [ ] Does the change keep one business boundary (or one data structure) per module, and is the new
      module named after that boundary rather than a technical layer?
- [ ] Are all touched `.rs` files within the 300–500 line budget (hard cap 500), and did an
      oversized file get split instead of extended?
- [ ] Any new `.clone()`, `String`, `collect()`, or `Box<dyn Trait>` field on the request path?
      Justify or remove.
- [ ] Any `expect`/`unreachable!` without a local invariant comment and `#[expect(...)]`?
- [ ] Are all new awaits bounded by a deadline and cancellation-aware? Any guard held across
      `.await`?
- [ ] Are all channels bounded, and is the overflow behavior explicit?
- [ ] Is any `Rc`/`RefCell`/`Cell` reachable from a `Send` future or shared state?
- [ ] Any new `pub` API? Does it have `///` with `# Errors`/`# Panics` as applicable, a typed error,
      and no leaked internal types?
- [ ] Does reload failure keep last-known-good, and does an in-flight request keep its generation?
- [ ] Is retry/fallback gated on "response not committed" and a retry budget?
- [ ] Do new error variants map to a decision at the edge layer, and are they logged exactly once?
- [ ] Are new logs structured, at the right level, and free of secrets/bodies?
- [ ] Do new Cargo features stay additive, and does the feature matrix build?
- [ ] Do tests cover the failure path, and are the errors themselves asserted?
- [ ] Are the five baseline commands green (including `RUSTDOCFLAGS="-D warnings" cargo doc`)?

---

## Common Mistakes

| Mistake | Fix |
|---|---|
| Reporting done with `cargo clippy` warnings outstanding | `-D warnings` is part of done |
| Believing `cargo doc` fails on broken intra-doc links | Add `RUSTDOCFLAGS="-D warnings"` |
| Treating `Bytes` as `Copy` | It is a cheap `Clone`; pass by value for that reason, not `Copy` |
| Adding a `pub` item without docs and letting `missing_docs` warn forever | Document it, or keep it private |
| Silencing pedantic/nursery noise with `#[allow]` | Gate CI on `clippy::all` first; track pedantic as backlog |
| Writing doc comments without `# Errors` on a `Result` API | Add the section; `missing_errors_doc` will catch it |
| Testing only the success path | Assert the `Err` variant and its message too |
| Using `sleep` to test debounce or timeout | `tokio::time::pause()` + `advance` |
| Snapshotting a whole config object with digests and timestamps | Snapshot the subtree and redact the unstable fields |
| Putting a long "what/how" comment block above a function | Split the function; put usage in `///` |
| Growing a module past 500 lines "because it is already big" | Split by boundary first; keep the split commit separate from behaviour changes |
| Adding `utils.rs` / `common.rs` / `types.rs` | Name the capability; a layer-named module hides the real boundary |
| Passing a handle or a callback across a module boundary to avoid a parameter | Pass data or call through the owning seam; handles couple lifetimes |
| Adding a feature that changes behavior of the default build | Features add capability; behavior is config |
| "It compiles, ship it" for an `unsafe` block | `unsafe` needs a dedicated task, `// SAFETY:`, and Miri |
| Optimizing without a profile | Measure first; ignore <5% wins |
| `Box::new([0u8; 65536])` for a large buffer | `vec![0; 65536].into_boxed_slice()` |
