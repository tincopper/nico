# Directory Structure

> How backend (Rust) code is organized in this project.

---

## Overview

The backend is **one Cargo Workspace** at the repository root. The layout follows three rules:

1. **Module before crate** — start inside a single crate. Split into a new crate only when a real
   boundary appears: independent Cargo feature, heavy/conflicting dependency, separate release,
   separate team, or compile isolation that clearly pays for itself.
2. **Dependency direction is enforced, not suggested** — `nico-core` never depends on a protocol
   crate or on Pingora; `nico-server` is the only crate that knows every concrete implementation.
3. **One binary target** — `nico` (built by the `nico-server` crate). Deployment roles (traffic / AI) are selected by Cargo
   features plus runtime config, not by adding more binaries.

> **Status**: `nico-core`, `nico-config`, `nico-http` and `nico-server` exist as built. The
> remaining crates below are the binding target structure. Architecture rationale lives in
> `.trellis/tasks/11-03-unified-gateway-analysis/design.md` — do not re-derive it from this file.

---

## Directory Layout

```text
nico/
├── Cargo.toml                  # [workspace] members, workspace deps, workspace lints
├── Cargo.lock                  # committed (binary workspace)
├── rust-toolchain.toml         # pinned toolchain
├── rustfmt.toml
├── clippy.toml                 # only for non-lint config (e.g. msrv, thresholds)
├── deny.toml                   # cargo-deny: advisories, licenses, duplicate versions
├── config/
│   ├── bootstrap.yaml          # local-only: config source connection, cache path, limits
│   └── resources.yaml          # gateway resources when using the local config source
└── crates/
    ├── nico-server/         # the only binary crate (binary `nico`): wiring, reload pipeline
    │   └── src/
    │       ├── main.rs         # thin: parse args, call lib::run, map error to exit code
    │       ├── lib.rs          # //! doc + pub fn run(...); makes wiring integration-testable
    │       ├── bootstrap.rs
    │       ├── reload.rs       # watch loop + debounce + single-flight + publish
    │   └── tests/              # integration tests reach the wiring through lib.rs
    ├── nico-core/           # small kernel shared by every data plane
    │   └── src/
    │       ├── lib.rs          # //! crate doc + #![deny(missing_docs)]
    │       ├── config.rs
    │       ├── context.rs
    │       └── error.rs
    ├── nico-http/           # Pingora integration + HTTP domain traits
    │   └── src/
    │       ├── lib.rs          # //! + the public contract (HttpError, HttpFilter, Destination, …)
    │       ├── context.rs      # HttpContext: snapshot, route, destination, deadline, budgets
    │       ├── error.rs        # HttpError + the single (status, code) mapping
    │       ├── filter.rs       # HttpFilter trait + FilterChain (budget-bounded stages)
    │       ├── limits.rs       # crate-private: body and time budget arithmetic
    │       ├── proxy.rs        # HttpApp + the unified rejection path
    │       ├── proxy/          # hooks.rs, failure.rs, forward.rs, record.rs (the Pingora contract)
    │       ├── route.rs        # match_route(header, routes)
    │       ├── service.rs      # ServerConf derivation + ProxyServiceBuilder + listener
    │       ├── settings.rs     # HttpSettings: bootstrap `http` defaults and ranges
    │       └── upstream.rs     # crate-private: Destination → HttpPeer
    ├── nico-ai/             # AI domain traits, providers, stream adapters
    │   └── src/
    │       ├── lib.rs
    │       ├── filter.rs
    │       ├── model.rs
    │       ├── provider.rs      # Provider trait + registry of built-in providers
    │       ├── provider/
    │       │   ├── openai.rs
    │       │   └── anthropic.rs
    │       └── stream/
    │           ├── sse.rs
    │           └── anthropic.rs
    ├── nico-config/         # ConfigSource trait, local source, LKG cache, fakes
    │   └── src/
    │       ├── lib.rs
    │       ├── source.rs
    │       ├── revision.rs
    │       ├── local.rs
    │       ├── cache.rs
    │       └── fake.rs
    ├── nico-config-etcd/    # optional, behind feature `config-etcd`
    │   └── src/lib.rs
    ├── nico-config-nacos/   # optional, behind feature `config-nacos` (later milestone)
    │   └── src/lib.rs
    └── nico-plugins/        # built-in HTTP policies: JWT, API key, rate limit
        └── src/
            ├── lib.rs
            ├── jwt.rs
            ├── api_key.rs
            └── rate_limit.rs
```

Per-crate test helpers live in `tests/` (integration) or in a `#[cfg(test)]` module inside the
file under test. Shared test utilities for one crate go in `src/test_support.rs` behind
`#[cfg(any(test, feature = "test-support"))]` — never in a separate published crate.

`nico-server` is the one crate with both `main.rs` and `lib.rs`. `main.rs` stays a thin shim
(parse args → call `lib::run` → map the error to an exit code) so the wiring, reload pipeline and
role selection are reachable from `tests/`. A binary-only crate cannot be integration-tested.

---

## Module Organization

### Module declaration style

Prefer the **`foo.rs` + `foo/` directory** form:

```text
provider.rs          # mod provider { pub mod openai; pub mod anthropic; }
provider/
├── openai.rs
└── anthropic.rs
```

Do **not** add `provider/mod.rs` when `provider.rs` already exists; `mod.rs` files are allowed only
in directories that already use them. Avoid deep nesting: three module levels below `src/` is the
practical ceiling before a submodule should become its own crate or be flattened.

Each module owns **one** business boundary or **one** data structure, and each `.rs` file stays
within the 300–500 line budget (hard cap 500, see `quality-guidelines.md`). When a file approaches
the cap, keep the parent as the entry point and move cohesive groups into `foo.rs` + `foo/`
submodules. Name modules after the capability (`debounce`, `publish`, `cache`), never after a layer
(`utils`, `helpers`, `types`, `common`).

### Where new code goes

| Kind of code | Location |
|---|---|
| HTTP/Pingora lifecycle, route matching, `HttpFilter`, upstream TLS policy | `nico-http` |
| Signals, runtime ownership, shutdown token, drain wiring | `nico-server` |
| AI provider semantics, model mapping, SSE translation | `nico-ai` |
| Config source protocol (etcd/Nacos clients) | `nico-config-*` (own crate, own feature) |
| Built-in HTTP policy (auth, rate limit) | `nico-plugins` |
| Wiring, feature selection, reload pipeline, shutdown | `nico-server` |
| Anything two data planes genuinely share (trace id, deadline, config generation, error category) | `nico-core` |

If you cannot name the second real consumer of an abstraction, it does **not** belong in
`nico-core`. One implementation behind a trait is a warning sign, not a design.

### Dependency rules

```text
nico-server
   ├──> nico-core
   ├──> nico-http ──────────> nico-core + pingora + tokio (time/net/rt; never signals)
   ├──> nico-ai ────────────> nico-http + nico-core
   ├──> nico-config ────────> nico-core
   ├──> nico-config-etcd ───> nico-config     (feature: config-etcd)
   ├──> nico-config-nacos ──> nico-config     (feature: config-nacos)
   └──> nico-plugins ───────> nico-http + nico-core
```

Hard rules:

- `nico-core` must not depend on `nico-http`, `nico-ai`, `nico-plugins`, `pingora`,
  `hyper`, or any config-center client.
- `nico-http` must not import `nico-ai`. AI plugs in through HTTP extension points: a filter
  resolves a provider target and records a `Destination`, and this crate owns only how to reach it.
- `nico-http` must not install signal handlers or build a runtime; that is process assembly, and
  it lives in `nico-server`.
- Config-center clients are never in the default dependency tree; they arrive only with their
  feature enabled.
- Dependencies between sibling data planes are added only after a design update in
  `design.md` — not as a convenience import.

### Visibility

- Default to private. Widen deliberately: `pub(crate)` for intra-crate reuse, `pub` only for the
  crate's public contract.
- Library crates carry `#![deny(missing_docs)]` at the crate root (workspace-level `missing_docs`
  stays off so the binary is not forced to document internals).
- Re-export only what external crates should name: `pub use source::{ConfigSource, ConfigVersion};`
  in `lib.rs`, not a blanket `pub use *`.
- Public enums that may gain variants get `#[non_exhaustive]`.

### Import grouping

`std` → external crates → workspace crates → `super::` → `crate::`, with a blank line between
groups. This is enforced by `rustfmt.toml` (`group_imports = "StdExternalCrate"`,
`imports_granularity = "Crate"`), which requires `cargo +nightly fmt` — see
`quality-guidelines.md`.

---

## Naming Conventions

| Thing | Rule | Example |
|---|---|---|
| Crate | `nico-<domain>` kebab-case | `nico-config-etcd` |
| Module / file | `snake_case`, one concern per file | `reload.rs` |
| Trait | noun describing the role, in the owning crate | `HttpFilter`, `Provider`, `ConfigSource` |
| Type | `PascalCase`, no `Impl`/`Manager`/`Factory` prefixes | `CompiledRoute`, `RuntimeSnapshot` |
| Function | `snake_case`, verb first | `build_snapshot`, `publish` |
| Constructor | `new` / `with_config`; `try_new` when fallible | `RouteTable::try_new` |
| Error enum | `<Domain>Error` with `thiserror` | `ConfigSourceError` |
| Feature | lowercase, domain-scoped | `ai`, `config-etcd` |
| Async spawn / channel bound | explicit, no `unbounded` in the name | `config_event_channel(capacity)` |

Avoid framework-flavored names (`PluginManager`, `AbstractFactory`, `BaseHandler`, `*ServiceImpl`)
unless the type really manages several lifecycles or algorithms.

---

## Examples

- Adding an AI provider: new module in `crates/nico-ai/src/provider/`, strong-typed config,
  registration in `nico-server` bootstrap. No changes to `nico-core` or `nico-http`.
- Adding an auth filter: new module in `crates/nico-plugins/src/`, implements
  `nico_http::HttpFilter`, registered in `nico-server`. No changes to `nico-ai`.
- Adding a config center: new `nico-config-*` crate implementing
  `nico_config::ConfigSource`, feature-gated, registered in `bootstrap.rs`. No changes to the
  reload pipeline.

---

## Verification

```bash
# Dependency boundary checks (must produce no output)
cargo tree -p nico-core --prefix none | grep -E 'pingora|nico-http|nico-ai|etcd-client|nacos'
cargo tree -p nico-config --prefix none | grep -E 'pingora|nico-http|nico-ai|etcd-client'
cargo tree -p nico-http --prefix none | grep -E 'nico-ai'

# Feature matrix must build
cargo check --no-default-features
cargo check --features ai,builtin-plugins
cargo check --all-features

# Duplicate / conflicting heavy deps
cargo tree -d

# File budget: every .rs file must be <= 500 lines (prints violations)
find crates -name '*.rs' | xargs wc -l | awk '$1 > 500 && $NF != "total" {print}'
```
