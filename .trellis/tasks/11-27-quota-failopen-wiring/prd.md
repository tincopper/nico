# 接线 quota fail-open 策略与 degraded 指标

## Goal

把 bootstrap `quota_store.on_unavailable` 选择的策略传入请求 hook：`fail-open` 在共享存储故障时
放行请求并产生 degraded 指标与结构化日志；`fail-closed` 行为保持不变。这是共享 token budget
交付的收尾：装配层已经把策略返回（`quota_build::build_quota_store` 的第二个返回值），但 hook
目前硬编码 fail-closed，策略被丢弃。

## Background

- `quota_build::build_quota_store` 返回 `(Arc<dyn BudgetStore>, UnavailablePolicy)`；
  `server.rs::serve_with` 目前把 policy 绑定为 `_unavailable_policy` 后丢弃。
- `upstream_peer`（`crates/nico-http/src/proxy/hooks/request.rs:133-151`）对存储错误硬编码
  `HttpError::BudgetStoreUnavailable`（fail-closed）。
- 策略是部署选择，与 store 生命周期相同：跨 reload 不变，从 `HttpStateHandle` 到请求上下文
  的既有传递路径（`budget_store()` → `set_budget_store`）是现成的载体。
- PRD（11-26-shared-token-budget R4）承诺：fail-open「继续请求，同时产生 degraded 指标和
  结构化日志，并记录配额绕过风险」。指标与日志目前缺位。

## Requirements

### R1 策略随 store 走

- `HttpStateHandle` 持有 `UnavailablePolicy`（本地默认 fail-closed，与进程内 store 不可能
  故障的事实一致）；`with_budget_store` 扩展为同时接收 policy（或新增独立 setter，实现期按
  现有构造形状选择，不破坏既有调用点）。
- `HttpContext` 增加 quota 策略字段，随 `set_budget_store` 一起在请求开始写入；
- `upstream_peer` 按策略分支：fail-closed 保持现行为（503 `quota_store_unavailable`）；
  fail-open 放行。

### R2 fail-open 可观测

- 放行时发一条 `warn!` 结构化日志：`quota_store_unavailable`、`policy = "fail-open"`、
  error 摘要；绝不包含 endpoint、凭证或 key 字节。
- 指标：`quota_store_degraded_total{policy}`（counter），每次 fail-open 放行递增；
  fail-closed 拒绝同样递增（`policy = "fail-closed"`），使运营者能看到存储健康状况——
  429 语义不变，指标是新增 family，不改变既有 family。

### R3 行为不变与文档

- 本地默认（无 `quota_store` section）行为零变化：进程内 store 不产生存储错误，
  fail-closed 分支不可达。
- charge 失败路径（`record_tokens`）已有 warn 日志；本任务不改变它。
- README 的 token budget 一节补一行：`on_unavailable` 的两个值及其含义。

## Acceptance Criteria

- [ ] `HttpStateHandle` 携带 policy 并在请求上下文可读；`with_budget_store` 调用点（含测试）
      编译通过。
- [ ] fail-closed：存储错误 → 503 `quota_store_unavailable`（现有行为，既有断言不变）。
- [ ] fail-open：存储错误 → 请求放行；`quota_store_degraded_total{policy="fail-open"}` 递增；
      `warn!` 日志出现且不含敏感值。
- [ ] fail-closed 拒绝同样递增 `quota_store_degraded_total{policy="fail-closed"}`。
- [ ] 单测：两种策略各一条 store-error 路径断言（fake store 注入故障）。
- [ ] README 补 `on_unavailable` 两值说明。
- [ ] `cargo +nightly fmt --all -- --check`、clippy `-D warnings`、
      `cargo test --workspace --all-features --locked`、
      `RUSTDOCFLAGS="-D warnings" cargo doc`、`cargo deny check`、feature 矩阵、行预算全绿。

## Out of Scope

- charge 失败的重试策略（保持现有单次 warn）。
- quota_store 的 README 完整配置示例（后续文档任务）。
- fail-open 的恢复探测/熔断（存储恢复后自然恢复，指标可见）。

## Open Questions

（无。策略载体与传递路径由现有代码形状决定。）
