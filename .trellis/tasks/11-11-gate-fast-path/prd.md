# pre-commit 门禁瘦身：去掉全量测试与冗余 check

## Goal

把 pre-commit 的耗时压回 `AGENTS.md` 与 `quality-guidelines.md` 要求的「fast path under a minute」量级，做法是把全量集成测试留给 pre-push，并把一次与 `cargo test` 重复的构建去掉。门禁的价值来自「每次提交都会跑」；一个要等 10–25 分钟的门禁实际会被绕过，那时它不保护任何东西。

## Background and Confirmed Facts

在本机（10 核 / 24 GB，磁盘 95% 满、swap 23 GB）实测，2026-11-11：

- `cargo check --workspace --all-targets --all-features`（热）**0.4s**；但紧跟一次 `cargo test` 之后同一条命令要重编 12 个 crate、**95s**（`sys` 时间 2m34s）。`cargo check` 只产出 `.rmeta`，`cargo test` 要代码生成，两者产物不共享。
- `cargo test -p <变更 crate 及反向依赖>` 在 `nico-server` 上展开为 **62 个测试套件 / 55 个二进制**，各套件 `finished in` 之和仅 **90s**，但整次 wall 在 **83s ↔ 709s ↔ 1700s+** 之间剧烈波动。
- 波动源不是测试逻辑：`real 537s` 时 `user` 仅 2.56s——进程 99.5% 的时间在等 I/O。`nico_config` 单元测试跨运行 14.8s → 23.7s → 35.4s → 134.5s；`local_file`（3 个测试）6.5s → 15.3s → 25.6s → 41.5s，这两个是文件监听测试，对 I/O 延迟最敏感。
- `target/` 已 **72 GB / 144 万文件**（`deps` 41 GB / 120 万文件，其中 683 个可执行文件共 16.8 GB；`incremental` 32 GB；重命名前的 `gateway_*` 残留 13,660 文件 / 1.7 GB）。冷启动 `cargo test -p nico-core --lib`（207 个测试、0.53s）要 **378s** wall；同一命令第二次 **4.85s**、第三次 **0.39s**。
- 现有 `scripts/gates/pre-commit.sh` 的顺序是 fmt → `cargo check --workspace --all-targets --all-features` → `cargo test -p <变更 crate + 反向依赖>`。反向依赖闭包使「改 `nico-core`」等于「测整个 workspace」，这是设计使然（`nico-core` 是内核），但代价是每次都跑 `nico-server` 的全部 62 个套件。
- `scripts/gates/pre-push.sh` 已跑 `cargo test --workspace --all-features` 加 clippy / doc / deny / feature 矩阵。**全量测试在 pre-push 已被覆盖**，pre-commit 再跑一遍是重复。

## Requirements

### R1 pre-commit 只跑三件事

`scripts/gates/pre-commit.sh` 在暂存了 Rust 相关文件时依次执行：

1. **fmt**：`cargo +nightly fmt --all -- --check`（无 nightly 时回退 stable，保留现有行为）。
2. **check**：`cargo check --workspace --all-targets --all-features --locked`。
3. **变更 crate 的单元测试**：`cargo test --lib -p <变更 crate + 反向依赖> --all-features --locked`。

不再跑集成测试（`tests/` 目录下的目标）。文件预算检查与「src 改动但无测试改动」的软提醒保持不变。

### R2 单元测试仍然按反向依赖闭包展开

改动 `nico-core` 必须仍然测到 `nico-http` / `nico-server` 的单元测试：一个内核改动破坏下游的编译期或单元行为，pre-commit 是唯一能在提交时拦住它的地方。**收缩的是测试层级（lib → 去掉集成），不是被测 crate 的集合。**

`--lib` 的目标是「快」而不是「少测下游」：它保留反向依赖闭包，只去掉需要起真实 socket / 真实进程的集成目标。

### R3 `--lib` 必须真的只选单元测试

验证 `cargo test --lib -p nico-server` 只跑 `src/` 里的 `#[cfg(test)]` 模块，不包含 `tests/` 下的任何目标。若 `--lib` 在某些 crate 上仍触发集成目标（例如 `nico-server` 的 `[[bin]]`），用 `--bins --lib` 或显式 `--tests` 排除，并在脚本注释里写明为什么。

### R4 去掉重复的构建

`cargo check --workspace --all-targets` 与随后的 `cargo test` 不共享产物。两者必须保留一个：

- **保留 `cargo check`**：它覆盖**全部** workspace 目标（含未在变更闭包里的 crate 与全部 `tests/` 目标的类型检查），而 `--lib` 测试只覆盖变更闭包内的 lib。类型错误出现在一个没人改的 crate 的集成测试里时，只有 check 能拦住。
- **不保留 `cargo test` 的全量形态**：见 R1。

因此 R4 的结论是「保留 check，收缩 test」，而不是删掉 check。这一点必须在脚本注释里写清，否则后来者会以为 check 是冗余的而删掉它。

### R5 慢的时候要能说出来

脚本在每步前后打印该步耗时（`date +%s` 差值即可，不引依赖），并在总耗时超过阈值（建议 120s）时打印一行 warn，指明「这一步慢在哪」。目的：当下一次门禁又变慢时，读日志的人不必再像本次分析一样手动二分。

不因此让门禁失败——慢不等于错，但必须可见。

### R6 pre-push 承接被移走的东西

确认 `pre-push.sh` 已经跑全量测试；若没有，补上。`pre-push` 是「全量」的唯一归属，且它的说明文字必须与 pre-commit 的新分工一致（现在 `pre-commit.sh` 的头部注释与 `lefthook.yml` 的注释都描述旧分工）。

### R7 文档同步

- `AGENTS.md` 里关于钩子的描述（若有）更新为新的三件事。
- `.trellis/spec/backend/index.md` 的 "Local gates: lefthook" 一节：`pre-commit` 的 bullet 从「tests for the changed crates and their reverse dependencies」改为「unit tests only」，并说明全量在 pre-push。
- `lefthook.yml` 的 `pre-commit` job 注释同步。

### R8 `nico-config` 的单元测试不再驱动真实文件监听

门禁剩下的最大单项是 `nico-config` 的 `local::tests::watch`：11 个用例里 6 个真的起 `notify` 去等内核通知，
单独跑合计约 30s、并行后 18.7s，而 `local::` 之外的模块全是 0.00s。这些用例测的是**操作系统的行为**
（FSEvents 合并、原子 rename、symlink 交换、SIGHUP），不是仓库的决策；把它们放在 `src/` 的单元测试里，
等于让每次提交都去等内核。

划分按「决策 vs 观测」切：

- **留在单元测试**（`crates/nico-config/src/local/tests.rs`，`--lib` 覆盖）：`is_relevant` 的路径判定、
  `EventSink` 的背压与订阅者消失、`watch_paths` 的解析，以及 `run_watch` 的**数据处理**——
  用注入的触发通道驱动（`WatchTriggers` 两个来源都留空，触发通道成为唯一唤醒源），验证
  「一次触发 → 重新读取当前文档 → 交付 snapshot 或 error」，以及取消后任务结束。
- **移到集成测试**（`crates/nico-config/tests/local_watch.rs`，`--lib` 不覆盖）：原子 rename、删除后重建、
  启动后创建、Kubernetes 风格 symlink 交换、SIGHUP。

移动不减少覆盖：那 5 个场景仍被执行，只是由 `pre-push` 的全量测试跑，而不是每次提交。

## Out of Scope

- **清理 `target/`**（72 GB / 144 万文件）与机器层面的 swap / Spotlight 问题：那是环境处置，不是仓库内容；本次分析已记录数据，处置另行决定。
- 引入 `cargo nextest`、`sccache`、`-Zthreads` 或任何新工具：没有测量支撑之前不加。
- 改变 pre-push 的检查集合（除 R6 的确认/补齐）。

## Acceptance Criteria

- [ ] 只暂存文档（无 Rust 文件）时，pre-commit 跳过 Rust 门禁（现有行为不变）。
- [ ] 暂存 Rust 文件时，脚本依次执行 fmt、check、`--lib` 测试，**不执行**任何 `tests/` 目录下的集成目标。可用 `cargo test --lib -p nico-server` 的输出证明：只有 `Running unittests src/lib.rs`，没有 `Running tests/*.rs`。
- [ ] 改动 `nico-core` 时，`--lib` 测试的 crate 集合仍包含 `nico-http` / `nico-server` 等反向依赖（打印出将被测试的 crate 列表，并由一条断言/输出核对）。
- [ ] 在热构建下（`target/` 已就绪），一次 pre-commit 的**总耗时**打印在日志里，且 `nico-server` 那一档不再出现 62 个套件的 `finished in` 行。
- [ ] 每一步打印耗时；总耗时超过 120s 时打印一行 warn 指明慢的步骤。
- [ ] `pre-push.sh` 仍执行全量 `cargo test --workspace --all-features`，且其头部注释描述新的分工。
- [ ] `scripts/gates/pre-commit.sh` 头部注释、`lefthook.yml` 的 job 注释、`.trellis/spec/backend/index.md` 的 lefthook 小节三处描述一致。
- [ ] 一次真实的 `git commit` 走完整钩子并成功（用本任务自己的提交验证），日志里能看到三个步骤各自耗时。
- [ ] `cargo test -p nico-config --lib` 的**套件内耗时**从约 20s 降到 1s 以内，且用例数不减少覆盖：被移走的 5 个场景在 `cargo test -p nico-config --test local_watch` 中全部通过。
- [ ] `local::tests` 里不再出现 `notify`、`TempDir` 之外的监听等待，也不再出现 10s 级预算：`WAIT` 只用于注入触发的一次调度往返。
- [ ] 新增的 `run_watch` 单测有变异检查证据：把生产代码改成交付错误事件后，`a_trigger_delivers_the_current_document_as_a_snapshot` 与 `a_trigger_re_reads_the_document_rather_than_replaying_the_first_read` 必须失败。

## Open Questions

（无——分工由实测数据决定：全量测试留在 pre-push，pre-commit 保留 check 以覆盖集成目标的类型检查。）
