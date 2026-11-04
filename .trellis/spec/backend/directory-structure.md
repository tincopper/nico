# Directory Structure

> How backend (Rust) code is organized in this project.

---

## Overview

The backend is **one Cargo Workspace** at the repository root. The layout follows three rules:

1. **Module before crate** — start inside a single crate. Split into a new crate only when a real
   boundary appears: independent Cargo feature, heavy/conflicting dependency, separate release,
   separate team, or compile isolation that clearly pays for itself.
2. **Dependency direction is enforced, not suggested** — `gateway-core` never depends on a protocol
   crate or on Pingora; `gateway-server` is the only crate that knows every concrete implementation.
3. **One binary target** — `gateway-server`. Deployment roles (traffic / AI) are selected by Cargo
   features plus runtime config, not by adding more binaries.

> **Status**: `gateway-core`, `gateway-config` and `gateway-server` exist as built. The remaining
> crates below are the binding target structure. Architecture rationale lives in
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
    ├── gateway-server/         # the only binary: wiring, reload pipeline, role selection
    │   └── src/
    │       ├── main.rs         # thin: parse args, call lib::run, map error to exit code
    │       ├── lib.rs          # //! doc + pub fn run(...); makes wiring integration-testable
    │       ├── bootstrap.rs
    │       ├── reload.rs       # watch loop + debounce + single-flight + publish
    │   └── tests/              # integration tests reach the wiring through lib.rs
    ├── gateway-core/           # small kernel shared by every data plane
    │   └── src/
    │       ├── lib.rs          # //! crate doc + #![deny(missing_docs)]
    │       ├── config.rs
    │       ├── context.rs
    │       └── error.rs
    ├── gateway-http/           # Pingora integration + HTTP domain traits
    │   └── src/
    │       ├── lib.rs
    │       ├── proxy.rs
    │       ├── route.rs
    │       ├── filter.rs
    │       ├── upstream.rs
    │       └── stream.rs
    ├── gateway-ai/             # AI domain traits, providers, stream adapters
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
    ├── gateway-config/         # ConfigSource trait, local source, LKG cache, fakes
    │   └── src/
    │       ├── lib.rs
    │       ├── source.rs
    │       ├── revision.rs
    │       ├── local.rs
    │       ├── cache.rs
    │       └── fake.rs
    ├── gateway-config-etcd/    # optional, behind feature `config-etcd`
    │   └── src/lib.rs
    ├── gateway-config-nacos/   # optional, behind feature `config-nacos` (later milestone)
    │   └── src/lib.rs
    └── gateway-plugins/        # built-in HTTP policies: JWT, API key, rate limit
        └── src/
            ├── lib.rs
            ├── jwt.rs
            ├── api_key.rs
            └── rate_limit.rs
```

Per-crate test helpers live in `tests/` (integration) or in a `#[cfg(test)]` module inside the
file under test. Shared test utilities for one crate go in `src/test_support.rs` behind
`#[cfg(any(test, feature = "test-support"))]` — never in a separate published crate.

`gateway-server` is the one crate with both `main.rs` and `lib.rs`. `main.rs` stays a thin shim
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

### Where new code goes

| Kind of code | Location |
|---|---|
| HTTP/Pingora lifecycle, route matching, `HttpFilter` | `gateway-http` |
| AI provider semantics, model mapping, SSE translation | `gateway-ai` |
| Config source protocol (etcd/Nacos clients) | `gateway-config-*` (own crate, own feature) |
| Built-in HTTP policy (auth, rate limit) | `gateway-plugins` |
| Wiring, feature selection, reload pipeline, shutdown | `gateway-server` |
| Anything two data planes genuinely share (trace id, deadline, config generation, error category) | `gateway-core` |

If you cannot name the second real consumer of an abstraction, it does **not** belong in
`gateway-core`. One implementation behind a trait is a warning sign, not a design.

### Dependency rules

```text
gateway-server
   ├──> gateway-core
   ├──> gateway-http ──────────> gateway-core + pingora
   ├──> gateway-ai ────────────> gateway-http + gateway-core
   ├──> gateway-config ────────> gateway-core
   ├──> gateway-config-etcd ───> gateway-config     (feature: config-etcd)
   ├──> gateway-config-nacos ──> gateway-config     (feature: config-nacos)
   └──> gateway-plugins ───────> gateway-http + gateway-core
```

Hard rules:

- `gateway-core` must not depend on `gateway-http`, `gateway-ai`, `gateway-plugins`, `pingora`,
  `hyper`, or any config-center client.
- `gateway-http` must not import `gateway-ai`. AI plugs in through HTTP extension points.
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
| Crate | `gateway-<domain>` kebab-case | `gateway-config-etcd` |
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

- Adding an AI provider: new module in `crates/gateway-ai/src/provider/`, strong-typed config,
  registration in `gateway-server` bootstrap. No changes to `gateway-core` or `gateway-http`.
- Adding an auth filter: new module in `crates/gateway-plugins/src/`, implements
  `gateway_http::HttpFilter`, registered in `gateway-server`. No changes to `gateway-ai`.
- Adding a config center: new `gateway-config-*` crate implementing
  `gateway_config::ConfigSource`, feature-gated, registered in `bootstrap.rs`. No changes to the
  reload pipeline.

---

## Verification

```bash
# Dependency boundary checks (must produce no output)
cargo tree -p gateway-core --prefix none | grep -E 'pingora|gateway-http|gateway-ai|etcd-client|nacos'
cargo tree -p gateway-config --prefix none | grep -E 'pingora|gateway-http|gateway-ai|etcd-client'
cargo tree -p gateway-http --prefix none | grep -E 'gateway-ai'

# Feature matrix must build
cargo check --no-default-features
cargo check --features ai,builtin-plugins
cargo check --all-features

# Duplicate / conflicting heavy deps
cargo tree -d
```
