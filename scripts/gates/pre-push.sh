#!/usr/bin/env bash
# Full baseline gate before a push.
#
# Invoked by lefthook (see lefthook.yml); safe to run directly.
#
# This is the **only** gate that runs the whole test suite, integration tests included: the
# pre-commit hook is limited to formatting, a workspace type check and the changed crates' unit
# tests, so that it stays fast enough to actually run on every commit. Everything the spec lists
# as "done" lives here: formatting, clippy with -D warnings, the whole test suite, docs, the
# feature matrix and cargo-deny when it is installed.
#
# Bypass for emergencies: SKIP_NICO_HOOKS=1 git push
set -euo pipefail

if [[ "${SKIP_NICO_HOOKS:-0}" == "1" ]]; then
    echo "[hook] SKIP_NICO_HOOKS=1: push gate skipped"
    exit 0
fi

root="$(git rev-parse --show-toplevel)"
cd "$root"

fail() {
    echo
    echo "[hook] FAILED: $1"
    exit 1
}

if rustup toolchain list 2>/dev/null | grep -q '^nightly'; then
    cargo +nightly fmt --all -- --check \
        || fail "formatting: run 'cargo +nightly fmt --all'"
else
    echo "[hook] warn: nightly toolchain missing, falling back to stable rustfmt"
    cargo fmt --all -- --check || fail "formatting: run 'cargo fmt --all'"
fi

cargo clippy --workspace --all-targets --all-features --locked -- -D warnings \
    || fail "clippy"

cargo test --workspace --all-features --locked \
    || fail "workspace tests"

# The feature matrix. `traffic` compiles without the AI crate, and the tests that pin that role are
# `#![cfg(not(feature = "llm"))]`: under `--all-features` they compile to nothing and pass without
# running, so the configuration that turns the feature off is the only gate that executes them.
cargo check --workspace --no-default-features --locked \
    || fail "feature matrix: no-default-features"
cargo check -p nico-server --no-default-features --features traffic --locked \
    || fail "feature matrix: traffic role"
cargo test -p nico-server --no-default-features --features traffic --test role_traffic --locked \
    || fail "feature matrix: the traffic role's own tests"

RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps \
    || fail "rustdoc (broken intra-doc links are denied)"

if command -v cargo-deny >/dev/null 2>&1; then
    cargo deny check || fail "cargo-deny (advisories, licenses, sources)"
else
    echo "[hook] warn: cargo-deny not installed; skipping dependency policy check"
fi

echo "[hook] push gate passed"
