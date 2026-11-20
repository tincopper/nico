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

> **Status**: `nico-core`, `nico-config`, `nico-http`, `nico-plugins`, `nico-llm`, `nico-metrics` and
> `nico-server` exist as built. The remaining crates below are the binding target structure. Architecture rationale
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
    │   ├── src/
    │   │   ├── main.rs         # thin: parse args, call lib::run, map error to exit code
    │   │   ├── lib.rs          # //! doc + pub fn run(...); makes wiring integration-testable
    │   │   ├── bootstrap.rs
    │   │   ├── bootstrap/      # http.rs (the listener list), inbound_tls.rs (one `tls` block: read + paired, the single implementation behind both a listener entry and the metrics section), proxy.rs (upstream trust roots), runtime.rs (the process knobs), source.rs (the acquisition subsystem), metrics.rs (the address, the collision against the data plane's set, and the optional TLS transport), tls.rs (root source + bundle check)
    │   │   ├── reload.rs       # watch loop, debounce, single flight, one publication
    │   │   ├── reload/         # error.rs, pacing.rs, publish.rs, startup.rs (one concern each)
    │   │   ├── metrics.rs      # transport: port preflight, recorder install, the assembly step
    │   │   ├── metrics/        # app.rs (ServeHttp adapter), listener.rs (one Pingora service: plaintext, or TLS built by nico-http's listener_tls with an http/1.1-only ALPN offer)
    │   │   ├── llm.rs          # the LLM plan one document compiles into
    │   │   └── llm/            # credentials.rs (a `credential_ref` read once per publication), error.rs (LlmBuildError), plan.rs (per-route front-ends, the phase/mode checks), runtime.rs (registries + resolver, created once per process)
    │   └── tests/              # integration tests reach the wiring through lib.rs
    │       ├── support/        # one server per test binary: bootstrap, source, client, mock upstream
    │       ├── llm_convert.rs  # OpenAI client → OpenAI provider: conversion, SSE, refusals, llm.* record
    │       ├── llm_convert/    # fixtures.rs: the documents and wire bodies the conversion scenarios share
    │       ├── llm_anthropic.rs # Anthropic client → OpenAI provider: one answer, one stream, one refusal, one record
    │       ├── llm_gemini.rs   # OpenAI client → Gemini provider: the pinned path, the folded answer, the 403, the refused passthrough
    │       ├── llm_passthrough.rs # byte-for-byte forwarding through the published endpoint
    │       ├── llm_publication.rs # the plan a document publishes, and what a refused one leaves alone
    │       └── tls_inbound.rs  # real TLS client verifies the listener; ALPN, per-connection scheme, plaintext + TLS side by side
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
    │       ├── context/         # identity.rs, buffer.rs, llm.rs, tests.rs: one data structure per file
    │       ├── error.rs        # HttpError + the single (status, code) mapping
    │       ├── error/          # rejection.rs (compiled policy refusal), llm.rs (a front-end's error document), respond.rs (how one failure renders)
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
    │       │   ├── hooks/      # {request,body,response,llm}.rs: per-hook delegation
    │       │   │   ├── request.rs
    │       │   │   ├── body.rs
    │       │   │   ├── response.rs
    │       │   │   └── llm.rs  # an AI route's two bodies: buffered once, translated per response
    │       │   ├── failure.rs  # classify + write_respond + write_synthetic, the only writers
    │       │   ├── forward.rs  # destination, Host/X-Forwarded-*, and the framing guard
    │       │   └── record.rs   # the request's one log record
    │       ├── route.rs        # match_route(header, routes)
    │       ├── service.rs      # ServerConf derivation + ProxyServiceBuilder + one registration per listener; a TLS entry is fallible
    │       ├── server_settings.rs # ServerSettings: the process knobs moved out of the http section
    │       ├── settings.rs     # HttpSettings: the listener list, h2c and the upstream trust policy
    │       ├── inbound.rs      # InboundTls, PEM identity validation, the static certificate resolver
    │       ├── state.rs        # HttpState/HttpStateHandle: snapshot + plan + llm routes, one store
    │       ├── state/          # plan.rs: the chains and the two per-route framing decisions
    │       ├── llm.rs          # the byte-level LLM seam: LlmRoutes/LlmRoute/ResponseTransform
    │       ├── llm/            # request.rs, response.rs, routes.rs: one data structure per file
    │       ├── upstream.rs     # crate-private: Destination → HttpPeer; `host` connects, `authority` names
    │       └── upstream/       # pools.rs (inheritance store, one generation's table, the selection
    │                           #   metric), pools/picker.rs (Strategy → Picker, exhaustive),
    │                           #   pools/health.rs (ejection: streak, cooldown, one probe),
    │                           #   pools/member.rs (identity, in-flight count, ConnectionGuard),
    │                           #   tls/: policy.rs (roots, no framework, no I/O) + adapter.rs (ServerConf)
    ├── nico-lb/             # member selection: the Picker contract and its strategies, no deps
    │   └── src/
    │       ├── picker.rs       # Picker/Members/View + the ordinal strategies, each owning its state
    │       ├── weighted.rs     # IpHash + WeightedRandom (distribution, not order)
    │       └── source.rs       # the injected randomness Source and the process-wide default
    ├── nico-llm/            # canonical LLM model + client codecs + provider adapters
    │   └── src/
    │       ├── lib.rs           # //! + builtin_registry(): the four codecs and four adapters this build serves
    │       ├── schema.rs        # canonical shape root; schema/ split by data boundary
    │       ├── schema/          # request/conversation/tool/extension/response/stream/usage/error/model/sampling
    │       ├── protocol.rs      # ClientCodec + ClientStreamEncoder traits, ClientReply (the status a client protocol answers with), registry
    │       ├── protocol/        # capabilities.rs (what a side can express), facts.rs (framework-free exchange facts), openai_error.rs (the two OpenAI codecs' shared envelope), then one module per wire dialect
    │       ├── protocol/openai_chat/       # {request,declaration,message,response,stream,error,usage}.rs
    │       ├── protocol/openai_responses/  # {request,items,response,stream,error,usage}.rs
    │       ├── protocol/anthropic/         # {request,blocks,response,stream,error,usage}.rs
    │       ├── protocol/gemini/            # {target,request,parts,response,stream,error,usage}.rs — `target` reads the model and operation out of the request target
    │       ├── provider.rs      # Provider/ProviderStream traits + registry
    │       ├── provider/        # endpoint.rs (the pinned model + RouteFact), route.rs (compiled route facts), base_url.rs (vendor-free URL rules), then one module per wire dialect
    │       ├── provider/openai/            # {url,request,response,stream,error,usage}.rs
    │       ├── provider/openai_responses/  # {request,response,stream,usage}.rs
    │       ├── provider/anthropic/         # {url,request,request/conversation,response,stream,stream/decoder,error,usage}.rs
    │       ├── provider/gemini/            # {url,request/request/conversation,parts,response,stream,fold,error,usage}.rs — `fold` rebuilds one buffered answer from the event stream
    │       ├── wire.rs          # crate-private shared wire helpers root: bytes and JSON, no vendor semantics
    │       ├── wire/            # base64.rs (RFC 4648 both directions), json.rs (parse, error class, reject_extra)
    │       ├── stream.rs        # declares the shared framing submodule
    │       ├── stream/
    │       │   └── sse.rs       # shared SSE framing (event boundaries, cross-chunk UTF-8, terminator), no vendor semantics
    │       ├── diagnostic.rs    # crate-private: what a refusal may quote (a bounded name, never a payload)
    │       ├── filter.rs        # route exchange: endpoint → decode → capability gate → model gate → encode → response/stream
    │       ├── filter/          # error.rs (failure vocabulary), stream.rs (one client encoder + one provider decoder per response)
    │       ├── route.rs         # the published front-ends as `nico_http::llm::LlmRoute`: ConvertRoute and PassthroughRoute
    │       └── route/           # convert.rs, passthrough.rs, transform.rs (one response's translation)
    ├── nico-metrics/        # self-reporting capability layer: no socket, no framework
    │   └── src/
    │       ├── lib.rs          # //! + the crate's four invariants (one recorder, caller's predicate, scrape-time upkeep, read-only)
    │       ├── recorder.rs     # install() (OnceLock) + render(): two scrape-time gauges, then upkeep, then render
    │       ├── health.rs       # Readiness: the live/ready projection and the JSON body
    │       └── response.rs     # Response + the request-line routing + the three path constants
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
| HTTP/Pingora lifecycle, route matching, the `Plugin` contract, upstream TLS policy and its Pingora adapter (`upstream/tls/{policy,adapter}`) | `nico-http` |
| The `Picker` contract, the strategies and the injected randomness source — one selection vocabulary reused by the traffic layer and the provider pool | `nico-lb` |
| Which member of a multi-address upstream a request connects to (`upstream/pools`: the inheritance store and one generation's table), and what the gateway has observed about that member | `nico-http` |
| The exhaustive `Strategy → Picker` and `PoolStrategy → Picker` matches (the document vocabulary stays with the crate that owns it) | `nico-http`, `nico-server` |
| Signals, runtime ownership, shutdown token, drain wiring | `nico-server` |
| LLM canonical schema, client protocol codecs, provider adapters, SSE translation | `nico-llm` |
| The byte-level seam an LLM front-end implements (destination, wire body, per-response transform) | `nico-http/src/llm.rs` |
| Config source protocol (etcd/Nacos clients) | `nico-config-*` (own crate, own feature) |
| Built-in HTTP policy (auth, rate limit) | `nico-plugins` |
| Wiring, feature selection, reload pipeline, shutdown | `nico-server` |
| Metric rendering, recorder installation, readiness projection (no socket, no framework) | `nico-metrics` |
| Anything two data planes genuinely share (trace id, deadline, config generation, error category) | `nico-core` |

If you cannot name the second real consumer of an abstraction, it does **not** belong in
`nico-core`. One implementation behind a trait is a warning sign, not a design.

### Upstream selection: the strategy and the state

`nico-lb` owns *the rule*; `nico-http/src/upstream/pools` owns *the observed history*. Neither knows
the other's internals, and both the traffic layer and the provider pool reuse the same selection
vocabulary.

| File | Owns | Must not |
|---|---|---|
| `nico-lb/src/picker.rs` | the `Picker` trait (`pick` + `inherit`), the `Members`/`View` accessors a rule may read, and the ordinal strategies with the state each of them keeps | know a `Destination`, an `HttpContext`, a `Strategy`, or how a member is reached. A new strategy is implementable without touching either consumer |
| `nico-lb/src/weighted.rs`, `source.rs` | the two distribution strategies (`IpHash`, `WeightedRandom`) and the injected randomness `Source` | count tokens or know a provider |
| `upstream/pools/picker.rs` | `build`: the one exhaustive `Strategy → impl` match | be duplicated in the selection crate: the document vocabulary belongs to the crate that owns it |
| `upstream/pools.rs` | the process-level inheritance store, the `SelectionTable` one generation serves from, member weights, and the selection counter | know any strategy's state layout: it holds `Arc<dyn Picker>` and passes facts in |
| `upstream/pools/health.rs` | the ejection machine: streak, cooldown, the single probe | know an upstream's name, log anything, or emit a metric |
| `upstream/pools/member.rs` | member identity, the in-flight count, and the `ConnectionGuard` that reports an outcome and logs it | decide availability (that is `health`'s verdict) and re-derive a member's name for a label |

`Strategy` in `nico-core` is deliberately **not** `#[non_exhaustive]`: it is a closed vocabulary the
data plane maps exhaustively, so a variant without a picker fails to compile instead of falling
through a default arm. `PoolStrategy` follows the same rule in `nico-server`.

### Provider pools: distribution, exclusions and the read budget

A route may name `providers` instead of one `provider`. Three pieces of long-lived or per-request
state make that work, and each has one owner:

| Piece | Owner | Contract |
|---|---|---|
| `PoolOrder` | `nico-llm/src/route/pool.rs` | The pooled members are the **prefix** of `AttemptChain`, so budgets, breakers and the fallback chain keep the rules they had. The strategy answers once per request — which member goes first — and the rest follow in pool order. `walk_order` filters what a request already tried, so one request never draws a member twice |
| `ExclusionTable` | `nico-llm/src/route/exclusion.rs`, held by `SharedServices` | Process-level, keyed by `(route, provider)` rather than `TargetIdentity`: the same provider may be a pool member on one route and not on another. Entries expire on read and are swept at publication. It is **parallel to, not merged into, the breaker** — a `429` retires a member for ten seconds, a service failure opens a breaker for the document's `cooldown_ms`; merging them would let one rate limit look like an outage |
| `Disposition` | `nico-llm/src/route/disposition.rs` | The failure classification runs **before** any counter moves: `429`/`401`/`403` retire a member without spending `retry_budget` or reporting to the breaker, `400`/`404`/`422` answer without asking another provider, and only a service failure reaches the budget |
| Pool model filtering | `PoolAllowlistMismatch` at publication; `walk_order` at request time | A pool's members must declare the **same** `model_allowlist` (or none). The target is chosen before the client's body arrives, so a request's model cannot narrow the member list first: members that disagree would route a request to a provider that must refuse it while another would have served it. The member's own `allows` check still runs at encode time; a retry round, where the model is known, filters the candidates |
| `ttft_timeout_ms` | `nico-core` schema → `peer.options.read_timeout` | Pingora sets the read bound once on the session and applies it to **every read**, header and body alike, and `upstream_peer` runs before the body is decoded. The field therefore means *how long one read of this route may take*, for streamed and buffered exchanges alike — not strictly "before the first byte". A read timeout on a route carrying this budget is the member's failure, not the request's deadline, so the walk replaces the member |

The token meter is a separate, response-time fact: `gateway_llm_tokens_total` counts only the
provider's **reported** buckets, under the physical model name. A kind the provider did not report
produces no sample. The gateway never counts tokens itself — there is no tokenizer, and a
self-counted number would never agree with the bill.

### Upstream TLS: two layers inside `nico-http`

`crates/nico-http/src/upstream/tls/` is split so the policy could move to its own crate without a
rewrite:

| File | Owns | Must not |
|---|---|---|
| `policy.rs` | the root source (`Platform`, or one `Bundle(PathBuf)` that **replaces** it), `TlsPolicy`, and `validate_bundle` — a pure check of PEM bytes | name any `pingora` identifier, or touch the file system: reading a file belongs to the caller, which is what makes every refusal testable on bytes |
| `adapter.rs` | the only mapping onto Pingora's `ServerConf` (`ca_file`); the only file in the crate that writes framework TLS configuration | carry a policy decision of its own |
| `mod.rs` | the module's public face, plus `ensure_usable` — the assembly-time probe that re-reads whatever source the policy names **and loads every entry the way the connector will** (`RootCertStore::add`, `validate_bundle`) | run per request: it reads files and belongs to startup |

TLS is a hard dependency of the crate, not a feature: there is no "build without TLS" product, and a
feature flag would keep a false branch (`refuse https`) alive for a shape nobody ships. `rustls` is
declared with `default-features = false` and only the provider `pingora-rustls` uses (`ring`); see
`pingora-guidelines.md` §3 for why a second provider is a defect rather than an addition.

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
   ├──> nico-plugins ───────> nico-http + nico-core
   └──> nico-metrics ───────> nico-core + metrics + metrics-exporter-prometheus
```

`nico-http` records through the `metrics` facade but never names `nico-metrics`: the exporter stays
out of the data plane's tree, which is the reason `nico-metrics` is a crate rather than a module.
`crates/nico-server/tests/dependency_boundaries.rs` asserts all three edges from the manifests.

Hard rules:

- `nico-core` must not depend on `nico-http`, `nico-llm`, `nico-plugins`, `pingora`,
  `hyper`, or any config-center client. It carries no network client and no policy cryptography:
  plugin credentials, signatures and caller-address hashes live in the crate that uses them; its one
  `sha2` use is `ConfigDigest`, a digest of the document bytes. That keeps adding a built-in policy
  from growing the kernel's dependency tree.
- `nico-http` must not import `nico-llm`. LLM support arrives through byte-level `LlmRoutes` and
  `ResponseTransform` traits: `nico-llm` produces a destination, a wire request and a per-request
  response transform; canonical types never cross into the data plane. This crate owns how to reach a
  peer, not what a provider speaks. `llm.rs` therefore names no vendor: a front-end answers with a
  `Destination`, a path, header pairs, opaque body bytes and the status it chose, and those trait
  methods are the only vocabulary the two crates share.
  For the same reason it must not import `nico-plugins`: a policy implements `Plugin` and reaches the
  request through `PluginContext` — a value type it never sees a framework type in — so the dependency
  edge points the other way.
- `nico-http` re-exports **no** Pingora type. The `Plugin` / `PluginContext` contract names only
  `String`, `&[u8]`, `Vec<u8>`, `u16`, `bool` and `u64` (`PluginContext::client_ip_hash() -> Option<u64>`),
  so a crate that implements a policy never depends on the HTTP framework: upgrading Pingora is one
  crate's task, and a WASM guest can mirror the same trait. `PluginChain` is the framework-facing
  driver and does name Pingora header types. The request target is `&[u8]` and not text on purpose: a
  policy that authorized on a lossily decoded target would be reading a target the router did not
  match, so the gateway hands over the bytes it routed on and a stage decides what undecodable ones
  mean.
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
- Wiring an LLM route into the data plane: `nico-llm` implements `nico_http::llm::LlmRoute` (one per
  route, publishing its endpoint as a `Destination`, a path, header pairs and a per-response
  `ResponseTransform`) and `nico-server::llm` compiles the plan: `LlmPlan::compile` resolves each
  provider route against `ProtocolRegistry`/`ProviderRegistry`, materializes its credential and refuses
  an unknown protocol or kind, an unimplemented mode, a passthrough pair whose protocols differ, a
  plugin body/response stage, a double-buffered route and an unaddressable endpoint — all as
  publication failures. `Reloader::publish_state` then hands the snapshot, the plugin plan and the LLM
  plan to `HttpStateHandle::publish_with_llm` in one store. Nothing in `nico-http` learns a vendor, a
  protocol name or a canonical type.
- Adding a client protocol: new module in `crates/nico-llm/src/protocol/`, implement `ClientCodec`
  (client wire ↔ canonical), register in `builtin_registry()`. No changes to `provider/`.
- A vendor name or a `match` on a provider kind is allowed only in `provider/<vendor>` and
  `protocol/<vendor>`; `schema/`, `filter.rs` and `stream/sse.rs` must not learn vendors. The
  canonical schema is the only vocabulary the two sides share. Two places may name a vendor, and
  only two: `protocol.rs` and `provider.rs` name their modules for the compiler, and
  `lib.rs::builtin_registry()` names each built-in once because assembling the set cannot be done
  without naming the types. `crates/nico-llm/tests/structure.rs` enforces the rule everywhere it
  matters — the canonical model, the exchange, the data plane — and records both exceptions.
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
last known good generation keeps serving. That covers every plugin instance the document declares
(even one no route names, whose line reports `scope = unused`) and every provider a route forwards
to; a provider entry no route references is not read at all, because an endpoint listed ahead of the
route that will use it cannot serve anything and must not be able to fail an unrelated publication.

---

## Verification

```bash
# Dependency boundary checks (must produce no output)
cargo tree -p nico-core --prefix none | grep -E 'pingora|nico-http|nico-llm|etcd-client|nacos|metrics'
cargo tree -p nico-config --prefix none | grep -E 'pingora|nico-http|nico-llm|etcd-client'
cargo tree -p nico-http --prefix none | grep -E 'nico-llm|nico-metrics|metrics-exporter-prometheus'
cargo tree -p nico-metrics --prefix none | grep -E 'pingora|tokio|nico-http'

# Feature matrix must build, and the role whose tests are cfg-gated must be tested
cargo check --workspace --no-default-features
cargo check -p nico-server --no-default-features --features traffic
cargo check -p nico-server --features llm
cargo check --workspace --all-features
cargo test -p nico-server --no-default-features --features traffic

# Add a feature to this matrix when one is introduced. `nico-server` declares the first ones
# (`llm`, on by default, and `traffic`); a test file gated on the absence of a feature compiles to
# nothing under `--all-features`, so the gate that turns the feature off is the only thing that runs it.

# Duplicate / conflicting heavy deps
cargo tree -d

# File budget: every .rs file must be <= 500 lines (prints violations)
find crates -name '*.rs' | xargs wc -l | awk '$1 > 500 && $NF != "total" {print}'
```
