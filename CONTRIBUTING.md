# Contributing

This repository is a Rust workspace for a unified traffic + AI gateway. Read
[`.trellis/spec/backend/`](.trellis/spec/backend/index.md) before writing code: it is the binding
contract for structure, errors, logging, tests and the file budget.

## Prerequisites

| Tool | Why | Notes |
|---|---|---|
| Rust | builds the workspace | `rust-toolchain.toml` pins `1.99.0`; rustup installs it on first `cargo` call |
| nightly toolchain + rustfmt | `cargo +nightly fmt` (import grouping is nightly-only) | `rustup toolchain install nightly --profile minimal && rustup component add rustfmt --toolchain nightly` |
| `cargo-deny` | license / advisory / source policy | `cargo install cargo-deny --locked` |
| `lefthook` | git hook orchestration | macOS `brew install lefthook`; elsewhere `go install github.com/evilmartians/lefthook@latest` |
| `cmake` | only for `poc/` (Pingora's zlib-ng build) | `brew install cmake` |
| `curl` with HTTP/2 | `poc/` probes | `curl --version` must list nghttp2 |

## First-time setup

```bash
# toolchains and tools (once per machine)
rustup toolchain install nightly --profile minimal
rustup component add rustfmt --toolchain nightly
cargo install cargo-deny --locked
brew install lefthook

# hooks (once per clone)
lefthook install
```

`lefthook install` writes `.git/hooks/pre-commit` and `.git/hooks/pre-push`. Do **not** set
`core.hooksPath`: it silently disables the hooks lefthook just installed.

Sanity check after setup:

```bash
lefthook version          # 2.2.2 or newer
git config core.hooksPath # must print nothing
```

## What the hooks do

| Hook | Scope | Checks |
|---|---|---|
| `pre-commit` | only when `*.rs`, `*.toml` or `*.lock` is staged | rustfmt check, `cargo check --workspace --all-targets`, tests for the changed crates **and their reverse dependencies**, the 500-line file budget for staged files, and a warning when `src/` changed without a test change |
| `pre-push` | every push | fmt, `clippy -D warnings`, `cargo test --workspace`, `cargo doc` with `RUSTDOCFLAGS="-D warnings"`, `cargo deny check` |

- Fix a red hook; do not bypass it. If a tool is unavailable, `SKIP_GATEWAY_HOOKS=1` skips the check
  and the bypass must be mentioned in the commit or PR.
- Hooks read the working tree, not the staged blob: stage everything the commit should contain.
- Run them manually with `lefthook run pre-commit` / `lefthook run pre-push`.
- The scripts behind the hooks live in [`scripts/gates/`](scripts/gates); `lefthook.yml` only decides
  when they run.

## Daily commands

```bash
cargo build --workspace --all-features
cargo test --workspace --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo +nightly fmt --all
cargo deny check
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

# run the gateway with the local file configuration source
cargo run -p gateway-server -- --bootstrap config/bootstrap.yaml
```

## Code rules

Full detail in [`.trellis/spec/backend/quality-guidelines.md`](.trellis/spec/backend/quality-guidelines.md)
and [`directory-structure.md`](.trellis/spec/backend/directory-structure.md). The short version:

- **Reason from invariants first.** State the invariants and failure modes before choosing a
  structure; every abstraction must name the coupling it removes and its second real consumer.
- **High cohesion**: one business boundary or one data structure per module. If describing a module
  needs "and", split it.
- **Low coupling**: dependencies point at the domain owner, never in cycles; pass data, not handles.
- **Extensibility at existing seams**: closed sets use `enum`, open ones use a domain trait; no
  registry/factory before the second implementation exists.
- **File budget**: 300–500 lines per `.rs` file, hard cap **500**. Split by boundary into
  `foo.rs` + `foo/` submodules instead of growing a file. Existing oversized files are refactor
  backlog — split before adding lines.
- Untyped errors, `unwrap`/`expect`/`panic!` outside tests, unbounded channels and glob imports are
  rejected in review (and by clippy).

## Commit and pull request conventions

- Use a type prefix: `feat:`, `fix:`, `docs:`, `chore:`, `test:`, `refactor:` (optionally scoped,
  e.g. `feat(config): ...`).
- Keep a commit to one purpose. File splits and behaviour changes go in separate commits.
- A bypassed hook (`SKIP_GATEWAY_HOOKS=1` or `--no-verify`) must be explained in the commit body.
- Work is tracked in Trellis tasks under `.trellis/tasks/`; planning artifacts are reviewed before
  implementation starts.

## Repository layout

```text
crates/gateway-core/     document model, revisions, snapshots, error categories
crates/gateway-config/   ConfigSource contract, local file source, LKG cache, fake
crates/gateway-server/   bootstrap, reload pipeline, CLI
config/                  bootstrap.yaml + resources.yaml for local runs
scripts/gates/           the checks behind the git hooks
poc/                     throwaway Pingora probes; not part of the workspace
```

`poc/` is a one-off verification asset. Do not copy its code into `crates/`; its conclusions live in
`.trellis/tasks/archive/2026-11/11-03-phase0-tech-verification/research/phase0-capability-report.md`.
