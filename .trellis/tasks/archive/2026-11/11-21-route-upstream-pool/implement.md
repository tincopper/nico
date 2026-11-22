# 执行计划：traffic 路由的 upstream 池（灰度分流）

## 前置

- 需求与设计：同目录 `prd.md`、`design.md`；冲突以 PRD 验收标准为准。
- 约定：Rust 走 `.trellis/spec/backend/`；不提交（用户明确要求时才 commit）。
- 关键事实：llm 池用 `nico_llm::route::select`（= nico-lb pickers 的再导出），两域本就共享
  选择契约；`PoolOrder::order` 只有一个调用点，client ip 穿线成本低。

## 清单

### 1. nico-core：schema 与引用

- [ ] `route.rs`：`PoolStrategy::IpHash`（枚举文档同步：ip_hash 两侧同义）；`UpstreamPoolMember`
      （`upstream` + `weight`）与 `UpstreamPool`（成员 + 策略，traffic 域自己的类型，
      不与 llm `Pool` 共用）；`Route::upstream_pool` 字段与访问器。
- [ ] `raw.rs`：`RawRoute.upstreams`（与 `upstream` 互斥）+ `RawUpstreamPoolMember`
      （weight 缺省 1）。
- [ ] `reference.rs`：目标命名矩阵改为"计数"（恰一种命名方式），新增
      `resolve_upstream_pool`——空池/重复成员/未声明引用/weight 0 或超界（`1..=1000`，
      traffic 域无 fallback）各自拒绝；池化路由 target = 池首成员（与 llm 同构）。
- [ ] `nico-llm` 不改语义：`PoolStrategy` 新变体只新增 `chain.rs` 的构建分支。

### 2. nico-llm：ip_hash 穿线

- [ ] `route.rs` 的 `select` 再导出补 `IpHash`。
- [ ] `PoolOrder::order(untried, names, client)`：client 进入 `View`；唯一调用点
      `attempt/pool.rs` 穿线（其调用方从 HttpContext 取 `client_ip`）。
- [ ] `nico-server/llm/plan/chain.rs`：`build_pool_picker` 补 `IpHash` 分支。

### 3. nico-http：两段选择

- [ ] `pools.rs`：`SelectionTable` 增加 `route_pools`（route id 键控）；
      `UpstreamPools` 增加对应的历史存储；`RoutePoolState`（成员名 + 权重 + picker）按代继承
      （策略不变则 picker 继承，规则与 upstream 成员池一致）；`Members` 适配
      （active = 0，池级无连接计数）。
- [ ] `pools/picker.rs`：traffic 侧 `PoolStrategy → Picker` 穷尽匹配。
- [ ] `UpstreamState::has_available(now)`：是否存在可用成员（路由池候选的过滤谓词）。
- [ ] `forward.rs::select_member`：两段选择——路由池（可用性感知）→ 成员池；无池路由走
      既有单目标路径。

### 4. 测试

- [ ] nico-core schema：互斥（upstream×upstreams、provider×upstreams）、空池、重复成员、
      weight 0/超界、未声明引用、strategy-无池；合法池 target = 池首成员。
- [ ] nico-http pools：weighted_random 比例（注入 `nico_lb::Source`）、round_robin 轮转、
      ip_hash 粘性与无地址回退、可用性感知（全员被剔出候选、全灭 502）、池状态按代继承。
- [ ] nico-llm：ip_hash 的粘性（client 进入 View 后同调用方稳定首选）。
- [ ] E2E：池路由两 upstream 各挂真实 mock，双侧都有流量；剔掉一侧后流量收敛。
- [ ] 既有全部测试不回归（llm 池默认 weighted_random 不变）。

### 5. 文档与 spec

- [ ] README：routes 节补 `upstreams` 池与 `strategy` 表（ip_hash 行）。
- [ ] `.trellis/spec/backend/directory-structure.md`：pools 所有权表补路由池一行
      （若需要）；llm/traffic 两域策略词汇的边界说明核对。
- [ ] `.trellis/spec/backend/error-handling.md`：无新错误类别（沿用发布期
      `DocumentError` 词汇），核对即可。

### 6. 质量门

- [ ] fmt / clippy `-D warnings` / workspace test / doc `-D warnings` / deny /
      feature matrix / 文件预算（无 `.rs` > 500）。

## 复审门槛

- 无池文档零行为变化；llm 池默认策略（weighted_random）不变。
- 路由池候选是唯一的 upstream 级健康裁决点；成员级裁决点不受影响。

## 回滚点

| 回滚到 | 方式 |
|---|---|
| 单目标路由 | 文档把 `upstreams` 池改写回 `upstream`；无 schema/数据迁移 |
