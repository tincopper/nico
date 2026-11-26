# 拆分 reload.rs 使其回到行预算内

## Goal

把 497 行的 `crates/nico-server/src/reload.rs` 拆回 500 行预算内，行为与公开 API 不变。
这是结构性工程债：下一个涉及 reload 行为的改动（TLS 热加载、Nacos 配置源、共享 quota 装配等）
都会触碰这个文件，先建立清晰边界可以避免在行为改动中越过文件硬上限。

## Background

- `AGENTS.md` 和 backend quality spec 规定每个 `.rs` 文件目标为 300–500 行，500 行是硬上限；接近上限时应先按职责拆分。
- `reload.rs` 当前 497 行，距上限只剩 3 行。
- 父模块已经按边界拆出 `error.rs`、`llm.rs`、`pacing.rs`、`publish.rs`、`startup.rs` 和 `tests.rs`；剩余主体是 `Reloader` 的状态、订阅、消费和恢复循环。
- 文件开头的三个类型组成一个独立的 reloader 输入边界：`Publication`、`ReloadLimits`、`ReloadSettings`。它们描述发布目标和运行参数，不持有 `Reloader` 状态，也不依赖其循环实现。
- `Publication` 和 `ReloadSettings` 的字段当前由父模块的 `Reloader::with_llm` 解构。移动到子模块后，Rust 的模块隐私要求父模块通过内部可见性访问这些字段；这只改变 crate 内部可见性，不改变外部 API。
- 模块文档的 `# Modules` 段需要补充新边界。

## Requirements

### R1 建立 settings 子模块

- 新增 `crates/nico-server/src/reload/settings.rs`，承载 `Publication`、`ReloadLimits`、`ReloadSettings` 的完整定义、文档注释、构造函数和访问器。
- `reload.rs` 声明 `mod settings;` 并通过 `pub use settings::{Publication, ReloadLimits, ReloadSettings};` 保持既有公开路径：
  `nico_server::reload::{Publication, ReloadLimits, ReloadSettings}`。
- `Publication` 和 `ReloadSettings` 中仅供 `reload` 父模块组装 `Reloader` 所需的字段，可调整为 `pub(super)`；字段不得对 crate 外公开。
- `Reloader` 对 `ReloadLimits` 的现有字段读取改用已有的 `debounce()` / `max_wait()` 访问器；不扩大 `ReloadLimits` 字段可见性。
- 现有外部调用方和 crate 内调用方无需修改 import；`nico_server` crate 根部的既有 re-export 继续生效。
- 更新 `reload.rs` 的模块文档，说明 `settings` 负责发布目标与 reloader 运行参数。
- 拆分后目标为 `reload.rs` ≤ 420 行、`settings.rs` < 500 行，且全树没有新增超过 500 行的 Rust 文件。

### R2 行为和 API 不变

- 不改变 reload 管道的函数签名、公开可见性、错误映射、指标、日志或状态机行为。
- 允许的代码变化仅为：模块声明与 re-export、类型从父文件移动到 `settings.rs`、为父模块访问移动后字段所需的 `pub(super)` 内部可见性调整，以及必要的 import 调整。
- 不新增测试；既有测试和编译结果证明结构移动没有改变行为。

### R3 README 状态措辞修正

- README「两个文件」表中 `resources.yaml` 行的「当前仅本地文件」已过时，因为 etcd 配置源已交付。
- 该行已修正为「由配置源提供（`local` 文件或 `etcd`）」；随本任务一并保留并验证。
- 不改 README 其他正文，也不改 `docs/nginx-migration.md` 中关于 bootstrap.yaml 本地读取的准确描述。

## Acceptance Criteria

- [ ] `crates/nico-server/src/reload/settings.rs` 存在，承载三个类型并包含模块级文档。
- [ ] `nico_server::reload::{Publication, ReloadLimits, ReloadSettings}` 公开路径不变；现有 crate 内和集成测试调用点无需修改 import。
- [ ] `Publication`、`ReloadSettings` 的内部字段访问可编译，且字段不对 crate 外公开。
- [ ] `reload.rs` ≤ 420 行；`settings.rs` < 500 行；以下行预算检查无输出：
      `find crates -name '*.rs' | xargs wc -l | awk '$1 > 500 && $NF != "total" {print}'`
- [ ] `cargo +nightly fmt --all -- --check` 通过。
- [ ] `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` 通过。
- [ ] `cargo test --workspace --all-features --locked` 通过。
- [ ] `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` 通过。
- [ ] `cargo deny check` 通过。
- [ ] 特性矩阵通过：
      `cargo check --workspace --no-default-features --locked`、
      `cargo check -p nico-server --no-default-features --features traffic --locked`、
      `cargo test -p nico-server --no-default-features --features traffic --test role_traffic --locked`、
      `cargo check -p nico-server --features llm --locked`、
      `cargo check --workspace --all-features --locked`。
- [ ] README 中不再出现「当前仅本地文件」，且只保留 `local` / `etcd` 的当前实现状态描述。

## Out of Scope

- 不改变 reload 管道的防抖、重试、LKG、发布、watch 或 shutdown 语义。
- 不拆 `Reloader` 的订阅/消费/恢复循环；这些逻辑共同维护一个状态机，settings 边界移出后主文件已有足够余量。
- 不新增测试、指标、配置字段、配置源、TLS 能力或共享 quota 实现。
- 不修改 bootstrap/config schema。
- `cache.rs` 当前 294 行，不在本任务范围。

## Open Questions

（无。三个类型共同表达 reloader 的发布目标与运行参数；父模块只需通过受限的 crate 内可见性组装 `Reloader`，不需要引入新的 trait、工厂或注册表。）
