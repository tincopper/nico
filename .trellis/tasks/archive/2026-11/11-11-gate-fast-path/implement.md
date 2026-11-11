# 执行计划：pre-commit 门禁瘦身

## 检查点

- **A（步骤 1–2）**：脚本改完，`--lib` 测试与耗时输出就位。此时可直接跑 `scripts/gates/pre-commit.sh` 验证。
- **B（步骤 3–4）**：文档三处同步，pre-push 分工确认。
- **收口（步骤 5）**：用本任务自己的提交走一遍完整钩子。

## 步骤

### 1. 收缩测试步骤

- [ ] `scripts/gates/pre-commit.sh` 第 3 步（"tests for changed crates and their dependents"）改为只跑 lib 目标：
      把 `cargo test $(for p in $packages; do printf -- '-p %s ' "$p"; done) --all-features --locked`
      改为 `cargo test --lib $(...) --all-features --locked`。
- [ ] 保留 `packages` 计算（反向依赖闭包）**不变**——收缩的是层级，不是 crate 集合。在注释里写明这一点，否则后来者会以为可以顺手简化成 `-p $(changed)`。
- [ ] 保留 `packages` 为空时的分支（workspace 级 manifest/toolchain 变更 → `cargo test --workspace --all-features`）。**这一条要改成 `--lib`**：workspace 级变更同样只需要单元测试在 pre-commit 拦住，全量在 pre-push。
- [ ] 头部注释的第 3 条从 "tests for the changed crates and their reverse dependencies" 改为 "unit tests (`--lib`) for the changed crates and their reverse dependencies; integration tests run in pre-push"。

### 2. 耗时可见

- [ ] 在脚本顶部加两个小函数：
      ```bash
      step_start() { STEP_NAME="$1"; STEP_STARTED=$(date +%s); echo "[hook] $STEP_NAME ..."; }
      step_done()   { local elapsed=$(( $(date +%s) - STEP_STARTED )); echo "[hook] $STEP_NAME: ${elapsed}s"; TOTAL=$(( TOTAL + elapsed )); }
      ```
      （`TOTAL` 与 `SLOW_GATE_SECONDS` 在顶部初始化，`SLOW_GATE_SECONDS="${SLOW_GATE_SECONDS:-120}"`。）
- [ ] 三步各包一层 `step_start` / `step_done`。
- [ ] 脚本末尾、`commit gate passed` 之前：
      ```bash
      if (( TOTAL > SLOW_GATE_SECONDS )); then
          echo "[hook] warn: gate took ${TOTAL}s (budget ${SLOW_GATE_SECONDS}s); the slowest step is printed above"
      fi
      echo "[hook] commit gate passed (${TOTAL}s)"
      ```
- [ ] 不引任何依赖；只用 `date +%s`（macOS 与 Linux 都有）。

### 3. 确认 pre-push 承接全量

- [ ] 读 `scripts/gates/pre-push.sh`，确认 `cargo test --workspace --all-features --locked` 存在（当前在第 38 行）。若缺失则补。
- [ ] 更新 `pre-push.sh` 头部注释：说明它现在是**唯一**跑全量测试的地方，而 pre-commit 只跑单元测试。
- [ ] 更新 `lefthook.yml` 的 `pre-commit` job 注释（现在写的是 "Only run when a Rust-relevant path is staged; documentation-only commits skip free"，需补一句分工）。

### 4. 文档同步

- [ ] `.trellis/spec/backend/index.md` 的 "Local gates: lefthook" 一节：
      `pre-commit` 的 bullet 从「tests for the changed crates **and their reverse dependencies**」改为「unit tests (`--lib`) for the changed crates and their reverse dependencies; the full suite runs in `pre-push`」。
- [ ] 同一节补一句：pre-commit 会打印每步耗时，超过 `SLOW_GATE_SECONDS`（默认 120s）时 warn——让「门禁变慢」这件事在日志里可见，而不是靠人二分。
- [ ] `AGENTS.md`：检查是否有钩子描述需要同步（当前只提到 `lefthook install` 与「a red pre-commit or pre-push hook must be fixed」，无需改；若发现描述旧分工则改）。

### 5. 用本任务自己的提交验证

- [ ] `cargo +nightly fmt --all` 与 `--check` 通过。
- [ ] 直接跑 `scripts/gates/pre-commit.sh` 一次，确认：
      - 输出里有 `fmt` / `check` / `unit tests` 三段各自的 `Ns`；
      - **没有** `Running tests/*.rs` 行（只有 `Running unittests src/lib.rs`）；
      - 末尾有总耗时。
- [ ] `SKIP_NICO_HOOKS=1` 不参与验证（不绕过）。
- [ ] 用本任务的提交走真实钩子，日志存档在提交说明里引用其总耗时。

## 收口验证

```bash
# 1. 只暂存文档：门禁应跳过
git add .trellis/tasks/11-11-gate-fast-path/prd.md
scripts/gates/pre-commit.sh   # 期望输出 "no Rust changes staged: skipping Rust gate"

# 2. 暂存 Rust 文件：三段耗时 + 无集成目标
git add scripts/gates/
scripts/gates/pre-commit.sh
# 期望：fmt / check / unit tests 各有 Ns；无 "Running tests/"
# 期望：末尾 "commit gate passed (Ns)"

# 3. --lib 真的只选单元测试
cargo test --lib -p nico-server --all-features --no-run 2>&1 | grep -c Executable   # 期望 1

# 4. 反向依赖闭包仍成立
# 脚本打印将被测试的 crate 列表；改动 nico-core 时期望包含 nico-http / nico-server
```

## 回滚点

- 步骤 1 后若发现 `--lib` 在某 crate 上仍触发集成目标：改 `--lib --bins` 或显式排除，并记录原因。不要退回全量。
- 若 pre-push 实际上没有跑全量测试：先补 pre-push，**再**收缩 pre-commit。顺序反了会有一段时间全量测试无人执行。
