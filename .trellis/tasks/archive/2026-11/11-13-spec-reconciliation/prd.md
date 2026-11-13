# 对齐 backend spec 与已交付子系统

## Goal

`.trellis/spec/backend/` 是 binding contract（`AGENTS.md`）。最近几轮交付了六个新子系统，
spec 里只有指标表跟上了，模块归属与不变量仍停在重构中途的形状。本任务把 spec 校准到
**当前最终设计**，让后续 agent 不会被旧名字误导。

不改代码。

## Background

已核实的具体漂移：

- `directory-structure.md:104` 写 `upstream/picker.rs`（Picker trait + 四种策略）——实际已抽成独立
  crate `crates/nico-lb/`，`nico-http` 与 `nico-llm` 都依赖它。
- `directory-structure.md:207` 的 "Where new code goes" 表仍把 `Picker` 归到 `nico-http`。
- `pingora-guidelines.md:202` 写 `[`Picker`](https://docs.rs)`，把内部 crate 链到了外部文档站。
- `ExclusionTable`（跨请求池成员退休）、`PoolOrder`（池前缀 + fallback）、`ttft_timeout_ms`
  （首字节/单次读预算）在 spec 里完全没有条目。
- `gateway_llm_pool_*` 与 `gateway_llm_tokens_total` 已进 `logging-guidelines.md` 的指标表
  （前几轮补过），但 `llm.tokens.*` 字段与「reported ≠ measured」的约束未进字段契约。
- `nico-lb` 未出现在 crate 归属表里。

## Requirements

### R1 修正名字与归属

- `directory-structure.md` 的目录树与 "Where new code goes" 表：加 `nico-lb` 一行，
  把 `Picker` 归属从 `nico-http` 改为 `nico-lb`；`upstream/` 下只保留 `pools*`、`tls/`。
- `pingora-guidelines.md`：去掉错误的 docs.rs 链接，写清 `Picker` 是 workspace 内 crate。
- 修正 `frontend/` 里无关的同名 "selector" 不碰（那些是 CSS/store selector，不是 LB）。

### R2 补新子系统的契约条目

在 `directory-structure.md` 的模块职责表（或等价位置）补：

- `nico-lb`：`Picker` / `Members` / `View` / `Source` 与六种策略；**不依赖任何 workspace crate**，
  配置词汇到实现的穷尽 match 留在依赖方。
- `nico-llm/route/exclusion.rs`：`ExclusionTable` 是进程级、键是 `(route, provider)`、与熔断器
  并列不合并；`Reason` 的冷却是类别属性。
- `nico-llm/route/pool.rs`：`PoolOrder` 让池成为 `AttemptChain` 的前缀，fallback 仍按文档顺序。
- `ttft_timeout_ms`：语义是「该路由一次读最多等多久」（Pingora `read_timeout` 作用于每次读），
  流式与非流式同样适用；这个约束必须写进 spec，否则后来者会按字段名误以为是严格 TTFT。

### R3 对齐 logging 契约

- `llm.tokens.*` 字段与「没有 usage 就不产生样本」进 `logging-guidelines.md` 的字段表。
- 明确 `gateway_llm_tokens_total` 只递增 reported，标签 `model` 是物理名；没有 `model_map` 时
  标签取请求里的名字（唯一基数可随流量增长的家族）。

### R4 遵守文档规则

- 设计文档只描述当前最终设计：不写「从 X 改成 Y」「修正了旧版本」「本次变更」之类的历史对比。
  修订 = 改写受影响的段落，不追加更正日志。
- 每个被改的段落自成一体，读者不需要前面的对话或旧草稿。

### R5 明确不做

- 不改任何 `.rs` 文件。
- 不改 `nico-core` 的 schema、不改 `Cargo.toml`、不改 README（README 已在各任务里更新）。
- 不新增 spec 文件（避免碎片化），只编辑既有四份。

## Acceptance Criteria

- [ ] `grep -rn "upstream/picker\|Picker.*docs.rs" .trellis/spec/backend/` 无输出。
- [ ] `nico-lb` 出现在目录树与 "Where new code goes" 表，且归属正确。
- [ ] `ExclusionTable` / `PoolOrder` / `ttft_timeout_ms` 在 spec 里有可读的契约描述。
- [ ] `llm.tokens.*` 与「reported ≠ measured / 无样本 ≠ 零样本」在 logging 契约里。
- [ ] 所有改动段落读起来像一直在那里，不含历史对比或更正日志。
- [ ] 产品代码零改动：`git status` 里 `crates/**/*.rs` 为空。
- [ ] 全树测试 / clippy / fmt 与改动前一致（文档改动不影响，但作为回归确认跑一次）。

## Out of Scope

- 实现任何新功能。
- 修订 roadmap 或已归档任务记录（那是历史，按文档规则保留原样）。
- frontend spec（与本任务无关）。

## Open Questions

（无。漂移点已由源码核实，逐条列在 Background。）
