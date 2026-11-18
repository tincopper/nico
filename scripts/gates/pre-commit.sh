#!/usr/bin/env bash
# Fast pre-commit gate for Rust changes.
#
# Invoked by lefthook (see lefthook.yml); safe to run directly.
#
# Runs only when a Rust-relevant file is staged, and does three things:
#   1. rustfmt check (nightly when available)
#   2. cargo check --workspace --all-targets (every target's types, test files included)
#   3. unit tests (`--lib`) for the changed crates and their reverse dependencies
#   4. file budget: reject staged .rs files that exceed 500 lines or grew past it
#   5. soft reminder when src/ changed without a test change
#
# What is deliberately **not** here: the integration suite (`crates/*/tests/`). Those targets
# start real sockets, real processes and real file watchers, which is the slowest thing this
# repository runs, and `pre-push.sh` already runs the whole suite. Moving them out is what keeps
# this gate under its one-minute budget; a gate nobody waits for protects nothing.
#
# `cargo check` is **not** redundant with the test step and must not be deleted: check covers
# every workspace target, including crates outside the change closure and every `tests/*.rs`
# file, while `--lib` covers only the closure's library. A type error in an untouched crate's
# integration test is caught by check and by nothing else before pre-push.
#
# Each step prints its own duration, and a total over `SLOW_GATE_SECONDS` (default 120) warns.
# That is not a failure — slow is not wrong — but it has to be visible, or the next slowdown costs
# someone another round of manual bisection.
#
# Bypass for emergencies: SKIP_NICO_HOOKS=1 git commit ...
# Full baseline (clippy, docs, deny, workspace tests) lives in scripts/gates/pre-push.sh.
set -euo pipefail

if [[ "${SKIP_NICO_HOOKS:-0}" == "1" ]]; then
    echo "[hook] SKIP_NICO_HOOKS=1: commit gate skipped"
    exit 0
fi

root="$(git rev-parse --show-toplevel)"
cd "$root"

# Seconds a whole gate may take before it says so. Overridable for a slow machine without editing
# the file, which is the only reason it is an environment variable.
SLOW_GATE_SECONDS="${SLOW_GATE_SECONDS:-120}"
TOTAL=0
STEP_NAME=""
STEP_STARTED=0

step_start() {
    STEP_NAME="$1"
    STEP_STARTED=$(date +%s)
    echo "[hook] $STEP_NAME ..."
}

step_done() {
    local elapsed=$(( $(date +%s) - STEP_STARTED ))
    echo "[hook] $STEP_NAME: ${elapsed}s"
    TOTAL=$(( TOTAL + elapsed ))
}

staged="$(git diff --cached --name-only --diff-filter=ACMR)"
if [[ -z "$staged" ]]; then
    exit 0
fi

rust_changed=0
while IFS= read -r file; do
    case "$file" in
        *.rs|Cargo.toml|Cargo.lock|rust-toolchain.toml|rustfmt.toml|clippy.toml|deny.toml|.cargo/*)
            rust_changed=1
            ;;
    esac
done <<< "$staged"

if [[ "$rust_changed" -eq 0 ]]; then
    echo "[hook] no Rust changes staged: skipping Rust gate"
    exit 0
fi

fail() {
    echo
    echo "[hook] FAILED: $1"
    echo "[hook] fix it, re-stage, and commit again"
    exit 1
}

# `protoc` is a build-script dependency rather than a Rust one: `etcd-client` compiles its own
# protos in `build.rs`, and `nico-config-etcd` generates the mock etcd server the same way. The
# check below builds `--all-features`, so both run even for a change that never names etcd. Without
# it the build dies inside a third-party panic, so name the tool and its installer here.
command -v protoc >/dev/null 2>&1 \
    || fail "protoc missing: 'brew install protobuf' on macOS, 'apt-get install protobuf-compiler' on Debian/Ubuntu"

# --- 1. formatting -----------------------------------------------------------
step_start "formatting"
if rustup toolchain list 2>/dev/null | grep -q '^nightly'; then
    cargo +nightly fmt --all -- --check \
        || fail "formatting: run 'cargo +nightly fmt --all'"
else
    echo "[hook] warn: nightly toolchain missing, falling back to stable rustfmt"
    cargo fmt --all -- --check \
        || fail "formatting: run 'cargo fmt --all'"
fi
step_done

# --- 2. syntax / types (all targets, so test code is covered) ----------------
# Kept even though step 3 builds too: check is the only step that sees the types of crates outside
# the change closure and of every `tests/*.rs` file. See the header for why that matters.
step_start "check"
cargo check --workspace --all-targets --all-features --locked \
    || fail "cargo check (syntax or type error)"
step_done

# --- 3. unit tests for changed crates and their dependents -------------------
# The crate set is the reverse-dependency closure: a change to nico-core still tests nico-config,
# nico-http and nico-server, because a kernel change that breaks a downstream is exactly what this
# step exists to catch at commit time. Only the *target kind* is narrowed — `--lib` — so the run
# stays inside `src/` and never starts a real socket, process or file watcher.
packages="$(python3 - "$staged" <<'PY'
import json, subprocess, sys

staged = [line for line in sys.argv[1].splitlines() if line.strip()]
meta = json.loads(subprocess.run(
    ["cargo", "metadata", "--no-deps", "--format-version", "1"],
    check=True, capture_output=True, text=True).stdout)

root = meta["workspace_root"]
pkgs = []
for pkg in meta["packages"]:
    base = pkg["manifest_path"][: -len("/Cargo.toml")]
    rel = base[len(root) + 1:]
    if any(f == rel or f.startswith(rel + "/") for f in staged):
        pkgs.append(pkg["name"])

# Reverse-dependency closure inside the workspace: a change to nico-core
# must also test nico-config and nico-server.
by_name = {pkg["name"]: pkg for pkg in meta["packages"]}
changed = set(pkgs)
while True:
    extra = {
        name for name, pkg in by_name.items()
        if name not in changed
        and any(dep["name"] in changed for dep in pkg["dependencies"] if dep.get("path"))
    }
    if not extra:
        break
    changed |= extra

print(" ".join(sorted(changed)))
PY
)"

if [[ -z "$packages" ]]; then
    # Workspace-level manifest or toolchain change: nothing maps to a crate, so every crate's
    # library is the unit under test. Still `--lib`: the integration suite belongs to pre-push.
    step_start "unit tests (whole workspace)"
    cargo test --workspace --lib --all-features --locked \
        || fail "workspace unit tests"
    step_done
else
    echo "[hook] unit tests for: $packages"
    step_start "unit tests"
    # shellcheck disable=SC2086 # word splitting is intended: one -p per package
    cargo test --lib $(for p in $packages; do printf -- '-p %s ' "$p"; done) --all-features --locked \
        || fail "unit tests for changed crates: $packages"
    step_done
fi

# --- 4. file budget (300-500 lines, hard cap 500) ----------------------------
python3 - "$staged" <<'PY' || fail "file budget: split the file before growing it past 500 lines"
import subprocess, sys

LIMIT = 500
staged = [line for line in sys.argv[1].splitlines() if line.strip()]

# A pure rename must be compared against the file it came from: the rule is "a file that is
# already over the cap may not grow", and moving a file does not grow it.
def rename_sources():
    out = subprocess.run(
        ["git", "diff", "--cached", "--name-status", "-M", "--diff-filter=R"],
        check=True, capture_output=True, text=True).stdout
    pairs = {}
    for line in out.splitlines():
        fields = line.split("\t")
        if len(fields) == 3:
            pairs[fields[2]] = fields[1]
    return pairs

renames = rename_sources()

def head_lines(path):
    source = renames.get(path, path)
    try:
        return int(subprocess.run(
            ["git", "show", f"HEAD:{source}"],
            check=True, capture_output=True, text=True).stdout.count("\n"))
    except subprocess.CalledProcessError:
        return None  # new file

violations, warnings = [], []
for path in staged:
    if not path.endswith(".rs"):
        continue
    try:
        with open(path, encoding="utf-8") as handle:
            current = handle.read().count("\n")
    except FileNotFoundError:
        continue  # deleted
    if current <= LIMIT:
        continue
    previous = head_lines(path)
    if previous is None:
        violations.append(f"{path}: new file has {current} lines")
    elif current > previous:
        violations.append(f"{path}: {previous} -> {current} lines (already over the cap)")
    else:
        warnings.append(f"{path}: {current} lines (pre-existing; do not add more)")

for warning in warnings:
    print(f"[hook] warn: {warning}")
if violations:
    for violation in violations:
        print(f"[hook] budget: {violation}")
    sys.exit(1)
PY

# --- 5. soft test reminder ---------------------------------------------------
src_changed=0
test_changed=0
while IFS= read -r file; do
    case "$file" in
        crates/*/src/*.rs) src_changed=1 ;;
        crates/*/tests/*.rs|*test*.rs) test_changed=1 ;;
    esac
done <<< "$staged"
if [[ "$src_changed" -eq 1 && "$test_changed" -eq 0 ]]; then
    echo "[hook] warn: source changed without a test change; confirm coverage is still honest"
fi

if (( TOTAL > SLOW_GATE_SECONDS )); then
    echo "[hook] warn: gate took ${TOTAL}s (budget ${SLOW_GATE_SECONDS}s); the per-step times above say which step"
fi

echo "[hook] commit gate passed (${TOTAL}s)"
