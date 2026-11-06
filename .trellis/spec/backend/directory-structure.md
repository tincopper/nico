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
3. **One binary target** — `nico` (built by the `nico-server` crate). Deployment roles (traffic / LLM) are selected by Cargo
   features plus runtime config, not by adding more binaries.

> **Status**: `nico-core`, `nico-config`, `nico-http`, `nico-plugins`, `nico-llm` and `nico-server`
> exist as built. The remaining crates below are the binding target structure. Architecture rationale
> lives in `.trellis/tasks/11-03-unified-gateway-analysis/design.md` — do not re-derive it from this
> file.

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
    │       ├── reload.rs       # watch loop, debounce, single flight, one publication
    │       └── reload/         # error.rs, pacing.rs, publish.rs, startup.rs (one concern each)
    │   └── tests/              # integration tests reach the wiring through lib.rs
    ├── nico-core/           # small kernel shared by every data plane
    │   └── src/
    │       ├── lib.rs          # //! crate doc + #![deny(missing_docs)]
    │       ├── config.rs
    │       ├── context.rs
    │       └── error.rs
    ├── nico-http/           # Pingora integration + HTTP domain traits
    │   └── src/
    │       ├── lib.rs          # //! + the public contract (HttpError, Plugin, Destination, …)
    │       ├── budget.rs       # crate-private: body and time budget arithmetic
    │       ├── context.rs      # HttpContext: snapshot, route, identity, destination, deadline, framing flags
    │       ├── context/         # identity.rs, buffer.rs, tests.rs: one data structure per file
    │       ├── error.rs        # HttpError + the single (status, code) mapping
    │       ├── error/          # rejection.rs: compiled refusal (status, template, category)
    │       ├── plugin.rs       # Plugin trait + Outcome/PluginInfo; re-exports ResponseData; the closed context contract
    │       ├── plugin/
    │       │   ├── chain.rs    # PluginChain/PluginEntry: the five stages under the remaining budget
    │       │   ├── chain/      # tests/{fixture,control,bytes}.rs: the driver's tests by subject
    │       │   ├── context.rs  # the data-plane PluginContext state and its phase guards
    │       │   ├── context/    # contract.rs (the trait impl), {phase,caller}.rs, tests.rs, tests/
    │       │   ├── error.rs    # PluginError + the one conversion into HttpError
    │       │   ├── header.rs   # header grammar and the hop-by-hop set a synthesized response is cleaned of
    │       │   ├── phases.rs   # PluginPhases: which stages a plugin declares (decides response framing)
    │       │   ├── registry.rs # PluginFactory/PluginRegistry: a document's `kind` becomes an implementation
    │       │   ├── response.rs # defines, validates and cleans ResponseData
    │       │   └── response/   # tests.rs: ResponseData validation and cleaning tests
    │       ├── proxy.rs        # HttpApp + the unified rejection path
    │       ├── proxy/          # the Pingora contract
    │       │   ├── hooks.rs    # the ProxyHttp impl: delegation only, one block per trait
    │       │   ├── hooks/      # {request,body,response}.rs: per-hook delegation
    │       │   │   ├── request.rs
    │       │   │   ├── body.rs
    │       │   │   └── response.rs
    │       │   ├── failure.rs  # classify + write_respond + write_synthetic, the only writers
    │       │   ├── forward.rs  # destination, Host/X-Forwarded-*, and the framing guard
    │       │   └── record.rs   # the request's one log record
    │       ├── route.rs        # match_route(header, routes)
    │       ├── service.rs      # ServerConf derivation + ProxyServiceBuilder + listener
    │       ├── settings.rs     # HttpSettings: bootstrap `http` defaults and ranges
    │       ├── state.rs        # HttpState/HttpStateHandle: snapshot + chains + llm routes + the two framing sets, one store
    │       └── upstream.rs     # crate-private: Destination → HttpPeer
    ├── nico-llm/            # canonical LLM model + client codecs + provider adapters
    │   └── src/
    │       ├── lib.rs
    │       ├── schema.rs        # canonical shape root; schema/ split by data boundary
    │       ├── schema/          # request/conversation/tool/extension/response/stream/usage/error/model/sampling
    │       ├── protocol.rs      # ClientCodec + ClientStreamEncoder traits, ClientReply (the status a client protocol answers with), registry
    │       ├── protocol/        # capabilities.rs (what a side can express), facts.rs (framework-free exchange facts), then openai_chat, openai_responses, anthropic, gemini
    │       ├── protocol/openai_chat/  # one codec's documents: {request,declaration,message,response,stream,error,usage}.rs
    │       ├── provider.rs      # Provider/ProviderStream traits + registry
    │       ├── provider/        # route.rs (compiled route facts), then openai, anthropic, gemini: wire ↔ canonical
    │       ├── provider/openai/  # one adapter's documents: {url,request,request/message,response,stream,error,usage,base64}.rs
    │       ├── stream.rs        # declares the shared framing submodule
    │       ├── stream/
    │       │   └── sse.rs       # shared SSE framing (event boundaries, cross-chunk UTF-8, terminator), no vendor semantics
    │       ├── diagnostic.rs    # crate-private: what a refusal may quote (a bounded name, never a payload)
    │       ├── filter.rs        # route exchange: endpoint → decode → encode → response/stream
    │       └── filter/          # error.rs (failure vocabulary), stream.rs (one client encoder + one provider decoder per response); model map lives on provider/route.rs
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
            ├── lib.rs       # //! + the publication-failure vocabulary (PluginError) + builtin_registry()
            ├── spec.rs      # a document entry compiled into the parameters a plugin runs with
            ├── rejection.rs # the compiled refusal of one instance + the built-in defaults
            ├── compile.rs   # document → global + per-route chains, buffered routes; registry lookup
            ├── <kind>.rs    # the `Plugin` implementation for one kind
            └── <kind>/      # `{config,factory,json,tests}.rs`: config, factory, helpers, tests
                ├── config.rs  # kind's own fields, defaults and rules
                ├── factory.rs # PluginFactory: config → spec → credentials → plugin
                ├── json.rs    # kind-specific JSON helpers when needed
                ├── limiter.rs # kind-specific policy helper when needed
                └── tests.rs   # the kind's tests
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
| HTTP/Pingora lifecycle, route matching, the `Plugin` contract, upstream TLS policy | `nico-http` |
| Signals, runtime ownership, shutdown token, drain wiring | `nico-server` |
| LLM canonical schema, client protocol codecs, provider adapters, SSE translation | `nico-llm` |
| Config source protocol (etcd/Nacos clients) | `nico-config-*` (own crate, own feature) |
| Built-in HTTP policy (auth, rate limit) | `nico-plugins` |
| Wiring, feature selection, reload pipeline, shutdown | `nico-server` |
| Anything two data planes genuinely share (trace id, deadline, config generation, error category) | `nico-core` |

If you cannot name the second real consumer of an abstraction, it does **not** belong in
`nico-core`. One implementation behind a trait is a warning sign, not a design.

Crate names state the capability, not the product. The product is an AI gateway, but there is no
`nico-ai`: LLM provider/stream semantics live in `nico-llm`, and a future MCP data plane becomes
`nico-mcp`. One protocol per crate keeps either from growing into the other's home.

### Dependency rules

```text
nico-server
   ├──> nico-core
   ├──> nico-http ──────────> nico-core + pingora + tokio (time/net/rt; never signals)
   ├──> nico-llm ────────────> nico-http + nico-core
   ├──> nico-config ────────> nico-core
   ├──> nico-config-etcd ───> nico-config     (feature: config-etcd)
   ├──> nico-config-nacos ──> nico-config     (feature: config-nacos)
   └──> nico-plugins ───────> nico-http + nico-core
```

Hard rules:

- `nico-core` must not depend on `nico-http`, `nico-llm`, `nico-plugins`, `pingora`,
  `hyper`, or any config-center client. It carries no network client and no policy cryptography:
  plugin credentials, signatures and caller-address hashes live in the crate that uses them; its one
  `sha2` use is `ConfigDigest`, a digest of the document bytes. That keeps adding a built-in policy
  from growing the kernel's dependency tree.
- `nico-http` must not import `nico-llm`. LLM support arrives through byte-level `LlmRoutes` and
  `ResponseTransform` traits: `nico-llm` produces a destination, a wire request and a per-request
  response transform; canonical types never cross into the data plane. This crate owns how to reach a
  peer, not what a provider speaks.
  For the same reason it must not import `nico-plugins`: a policy implements `Plugin` and reaches the
  request through `PluginContext` — a value type it never sees a framework type in — so the dependency
  edge points the other way.
- `nico-http` re-exports **no** Pingora type. The `Plugin` / `PluginContext` contract names only
  `String`, `Vec<u8>`, `u16`, `bool` and `u64` (`PluginContext::client_ip_hash() -> Option<u64>`),
  so a crate that implements a policy never depends on the HTTP framework: upgrading Pingora is one
  crate's task, and a WASM guest can mirror the same trait. `PluginChain` is the framework-facing
  driver and does name Pingora header types.
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
| Trait | noun describing the role, in the owning crate | `Plugin`, `Provider`, `ConfigSource` |
| Type | `PascalCase`, no `Impl`/`Manager`/`Factory` prefixes | `CompiledRoute`, `RuntimeSnapshot` |
| Function | `snake_case`, verb first | `build_snapshot`, `publish` |
| Constructor | `new` / `with_config`; `try_new` when fallible | `RouteTable::try_new` |
| Error enum | `<Domain>Error` with `thiserror` | `ConfigSourceError` |
| Feature | lowercase, domain-scoped | `llm`, `config-etcd` |
| Async spawn / channel bound | explicit, no `unbounded` in the name | `config_event_channel(capacity)` |

Avoid framework-flavored names (`PluginManager`, `AbstractFactory`, `BaseHandler`, `*ServiceImpl`)
unless the type really manages several lifecycles or algorithms.

---

## Examples

- Adding an LLM provider: new module in `crates/nico-llm/src/provider/`, implement `Provider` and
  `ProviderStream` (canonical ↔ vendor wire), register in `builtin_registry()`. No changes to
  `schema/`, `protocol/`, `nico-core` or `nico-http`.
- Adding a client protocol: new module in `crates/nico-llm/src/protocol/`, implement `ClientCodec`
  (client wire ↔ canonical), register in `builtin_registry()`. No changes to `provider/`.
- A vendor name or a `match` on a provider kind is allowed only in `provider/<vendor>` and
  `protocol/<vendor>`; `schema/`, `filter.rs` and `stream/sse.rs` must not learn vendors. The
  canonical schema is the only vocabulary the two sides share. The one repetition is a module
  declaration: `protocol.rs` and `provider.rs` must name `protocol/<vendor>.rs` for the compiler, so
  `crates/nico-llm/tests/structure.rs` allows a vendor name on a module-wiring line of those two
  registry roots and nowhere else.
- Adding an auth plugin: add `<kind>.rs` for the `Plugin` implementation and `<kind>/` with
  `{config,factory,json,tests}.rs` for its config, factory, helpers and tests, registered by
  `builtin_registry()`. No changes to `nico-llm`, and none to `nico-core` or `nico-http`.
- Adding a config center: new `nico-config-*` crate implementing
  `nico_config::ConfigSource`, feature-gated, registered in `bootstrap.rs`. No changes to the
  reload pipeline.

### Where a plugin boundary sits

A built-in plugin uses the `foo.rs` + `foo/` split: `<kind>.rs` holds the `Plugin` implementation and
`<kind>/{config,factory,json,tests}.rs` hold its config, factory, helpers and tests, plus one shared
decision surface:

| Concern | Owner |
|---|---|
| The envelope: `name`, `kind`, `enabled`, `buffers_body`, `error_response`, name uniqueness, and the `global_plugins` / `routes[].plugins` references | `nico-core::config` (`plugin.rs`, `semantic/plugins.rs`, `reference.rs`) |
| The kind's own `config`: field names, defaults, ranges, `env:`/`file:` references | `nico-plugins` (`<kind>/config.rs`, validated at publication) |
| The phase the kind runs in, its refusal, and materializing its credential | `nico-plugins` |
| Writing the response, the closed header set, `(status, code)` for everything else | `nico-http` |

`kind` is a string in the document model and the kind's `config` value is opaque there, so the
registry that answers "does this kind exist?" lives in `nico-plugins` and an unknown kind is a
publication failure, never a silent skip. That is what lets a second source of plugins arrive without
teaching the document model any policy: `nico-core` never learns what a phase is or what a kind's
fields are, and `nico-http` never learns what a plugin is.

`is_secret_ref` and `validate_host` (`nico-core::config`) are the vocabulary the crates share: the
document model checks a provider's `credential_ref` and every `host` field with them, and a provider
adapter checks the host it parsed out of a `base_url` with `validate_host`, so "a credential is a
reference" and "a host is a name" each have one spelling. The consequence is the point: an adapter
cannot produce a `Destination` the data plane would write as an unreadable `Host`, because the rule
that refuses one already exists and every producer applies it rather than a subset of it.

A credential reference is materialized when the document is compiled, not per request: `env:VAR`
reads the variable and `file:/path` reads the file **byte for byte**, so a trailing newline written
by `echo` is part of the key. An unreadable or empty credential rejects the whole document and the
last known good generation keeps serving.

---

## Verification

```bash
# Dependency boundary checks (must produce no output)
cargo tree -p nico-core --prefix none | grep -E 'pingora|nico-http|nico-llm|etcd-client|nacos'
cargo tree -p nico-config --prefix none | grep -E 'pingora|nico-http|nico-llm|etcd-client'
cargo tree -p nico-http --prefix none | grep -E 'nico-llm'

# Feature matrix must build (the workspace currently defines no features)
cargo check --no-default-features
cargo check --all-features

# Add a feature to this matrix when one is introduced.

# Duplicate / conflicting heavy deps
cargo tree -d

# File budget: every .rs file must be <= 500 lines (prints violations)
find crates -name '*.rs' | xargs wc -l | awk '$1 > 500 && $NF != "total" {print}'
```
