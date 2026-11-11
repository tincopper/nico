# 设计：pre-commit 门禁瘦身

## 1. 三个步骤，各自负责一件事

```
pre-commit（暂存了 Rust 文件时）
├── 1. fmt        cargo +nightly fmt --all -- --check
├── 2. check      cargo check --workspace --all-targets --all-features --locked
└── 3. unit test  cargo test --lib -p <变更 crate + 反向依赖> --all-features --locked
```

**为什么这三步、这个顺序**：

| 步骤 | 覆盖什么 | 为什么不能省 |
|---|---|---|
| fmt | 格式 | 秒级，且是唯一能在提交时阻止格式漂移的地方 |
| check | **全部** workspace 目标，含每个 `tests/*.rs` 的类型检查 | 只覆盖变更闭包的 `--lib` 测试看不见「没人改的那个 crate 的集成测试类型错误」 |
| unit test | 变更 crate 及其反向依赖的 `src/` 内测试 | 反向依赖闭包保证内核改动会测到下游；`--lib` 保证不起真实 socket |

**顺序理由**：fmt 最便宜先跑（失败最快给出），check 次之（全量类型，比测试便宜），测试最后（最贵）。任一步失败即停，后面的不跑——`set -euo pipefail` 已保证。

## 2. `--lib` 是真的只跑单元测试（已验证）

```
$ cargo test --lib -p nico-server --all-features --no-run
  Executable unittests src/lib.rs (...)
```

只 1 个目标。`nico-server` 有 `[[bin]] nico`，但 `--lib` 不选它。七个 crate 的 `--lib` 合计 907 个测试：

| crate | 测试数 | 套件内耗时 |
|---|---|---|
| `nico-config` | 53 | 41.0s ← 最大项（文件监听） |
| `nico-core` | 207 | 0.24s |
| `nico-http` | 293 | 0.02s |
| `nico-llm` | 73 | 0.01s |
| `nico-metrics` | 8 | 0.20s |
| `nico-plugins` | 175 | 0.11s |
| `nico-server` | 98 | 0.01s |

wall 164s（`user` 12.5s / `sys` 35.6s），即仍在等 I/O——`nico_config` 的 41s 和链接开销。**这是本机当前状态的产物，不是脚本的产物**；脚本改动把「62 个套件」降到「7 个 lib 目标」，剩下的波动是环境问题，已记录在 PRD 的 Background。

## 3. 收缩的是层级，不是 crate 集合

反向依赖闭包**原样保留**。改动 `nico-core` 时仍会测 `nico-config`、`nico-http`、`nico-llm`、`nico-plugins`、`nico-server` 的单元测试——这是 pre-commit 唯一能拦住「内核改动破坏下游」的地方。

被移走的只有 `tests/` 下的集成目标，它们需要真实 socket、真实进程、真实文件系统，正是本机 I/O 环境下最贵的那一类；而且 `pre-push.sh` 已经在跑全量 `cargo test --workspace --all-features`，它们在推送前仍被执行。

**为什么不干脆把 check 也删掉**：`cargo check --workspace --all-targets` 与 `cargo test --lib -p <闭包>` 覆盖不同集合——前者是全 workspace 全目标，后者是闭包内 lib。删掉 check 会漏掉「闭包外的 crate」和「所有集成测试文件」的类型错误，而这些错误在 pre-push 之前无人发现。保留 check 的代价是一次全量类型检查（热态 0.4s，冷态数十秒），远小于它覆盖的增量。

## 4. 耗时可见

每步前后打时间戳，打印该步耗时；总耗时超过 `SLOW_GATE_SECONDS`（默认 120）时打一行 warn，列出最慢的一步。

**为什么不直接失败**：慢不等于错。把它变成失败会迫使用户在慢机器上 `SKIP_NICO_HOOKS=1`，那才是真正的失效。可见性足够——本次分析正是因为没有这一步耗时输出，才需要手工二分 6 次。

## 5. `nico-config`：按「决策 vs 观测」切分单元测试

`--lib` 之后剩下的最大单项是 `local::tests::watch`：11 个用例里 6 个真起 `notify` 等内核通知。单独计时：

| 用例 | 耗时 |
|---|---|
| `watch_emits_a_snapshot_after_an_atomic_rename` | 11.70s |
| `watch_emits_a_snapshot_after_a_kubernetes_style_symlink_swap` | 6.58s |
| `dropping_the_stream_stops_the_watch_task` | 6.08s |
| `watch_reports_unavailable_after_deletion_and_recovers_on_recreation` | 5.67s |
| `watch_emits_a_snapshot_when_the_file_is_created_after_the_watch_started` | 5.64s |
| `watch_emits_a_snapshot_on_sighup` | 0.00s |
| `local::` 其余 15 个（load / construction / sink / relevance） | 合计 0.00s |

判据是**这条断言在测谁的行为**：

- 「一次原子 rename 后内核会不会通知我」测的是 `notify` 与 kernel/FSEvents，不是本仓库的决策。它需要一个真实
  文件系统、真实时间和真实后端差异，属于集成测试。
- 「一条通知该不该算作文档变化」（`is_relevant`）、「一次触发该不该重新读取并交付 snapshot」（`run_watch`）、
  「通道满了该等还是丢」（`EventSink`）都是本仓库的决策，可以用注入的输入在毫秒内验证。

### 注入缝隙已经存在，不需要改生产代码

`run_watch` 的签名把触发源拆成两部分：`triggers: mpsc::Receiver<()>` 与 `watchers: WatchTriggers`。
后者只持有 `notify` 后端和 SIGHUP 流。测试构造 `WatchTriggers { _watcher: None, sighup: None }`
并自己持有 `mpsc::Sender`，`changed()` 的两个分支就只剩通道这一路可选——触发完全由测试决定。

生产代码零改动，这是本次切分成本低的原因；也说明当初把触发源与后端分开就留对了缝。

### 移动而不是删除

被移走的 5 个场景仍原样执行，只是改由 `crates/nico-config/tests/local_watch.rs` 承载，`--lib` 不再选中它们。
`pre-push` 的全量 `cargo test --workspace` 会跑到，所以没有覆盖损失，只是**代价从每次提交挪到每次推送**——
这正是本次任务的分工。

`dropping_the_stream_stops_the_watch_task` 不是移动而是重写：它断言「任务结束」，而任务结束可以用取消令牌在
注入模式下验证，不必先等一次真实通知。重写后从 6.08s 变成毫秒级，且断言的还是同一个事实。

## 6. 被否掉的方案

| 方案 | 为什么否掉 |
|---|---|
| 整个门禁挪到 pre-push | pre-commit 变成只查格式，等于放弃提交时拦截；且 pre-push 未必每个开发者都装 |
| 用 `cargo nextest` 替代 | 未测量；nextest 不跑 doc test，需要额外配置，且本机瓶颈是 I/O 不是测试框架 |
| 只测变更 crate，不做反向依赖闭包 | 改动 `nico-core` 时下游破坏不会被发现，等于把内核当独立 crate |
| 把 `--lib` 换成 `--tests` | 方向相反：`--tests` 只跑集成测试，更慢 |
| 缓存测试结果（按文件 hash 跳过） | 引入状态与失效逻辑，风险高于收益；cargo 自身的增量已经做了这件事 |
