# 实施：模型层负载均衡 M1

前置：`11-11-upstream-lb` 已落地（`picker`/`pools` 可复用）。

## 第 1 步：配置面（`nico-core`）

- [x] `RawRoute` 加 `providers: Option<Vec<RawPoolMember>>` 与 `strategy: Option<PoolStrategy>`；
      保持 `deny_unknown_fields`。
- [x] `Route` 加 `pool: Option<Pool>`；`providers` 与 `provider` 互斥，两种错法各自一条错误
      （都写 / 都不写）。
- [x] 成员名必须命中已声明的 provider；池内不得重复；`weight` 在 `0..=1000`，缺省 `1`。
- [x] `schema_bounds` 新增 `max_pool_members`（默认 8），与 routes/upstreams/plugins 同一做法。
- [x] `MAX_ATTEMPTS_PER_ROUTE` 改为
      `MAX_RETRY_BUDGET * DEFAULT_MAX_POOL_MEMBERS * (1 + MAX_FALLBACK_MEMBERS)`，并新增
      `attempt_ceiling(schema)` 供提升过 `max_pool_members` 的文档取自己的上界。
- [x] 单测：互斥两种错法、未声明成员、重复成员、权重边界、池上限、乘积常量。

**检查点 A**：`cargo test -p nico-core --lib` 绿；既有文档 fixture 零改动解析通过。

## 第 2 步：抽取 `nico-lb`（复用流量层选择算法）

触发条件已满足：provider 池是 `Picker` 的第二个真实消费者。

- [x] 新建 `crates/nico-lb`：`Picker` trait、`Members`/`View`、四个既有实现、测试整体搬入；
      `pub(crate)` 改为 `pub`；crate 级 `#![deny(missing_docs)]`。
- [x] `nico-lb` **不依赖 `nico-core`**：配置词汇（`Strategy` → 实现）的映射留在依赖方。
      `nico-http` 保留既有 `build(Strategy)` 穷尽 match，`nico-server` 新增
      `build_pool(PoolStrategy)` 穷尽 match。
- [x] `Members` 增加一个访问器以支持模型过滤：候选集由调用方在构造 `View` 之前过滤，
      **trait 不新增"能否服务某模型"的方法**——那是 provider 的知识，不是成员的属性。
- [x] `View` 增加随机源入口（`weighted_random` 需要），或由 `weighted_random` 实现自己持有
      可注入的 `Source`（在 implement 时二选一，见 design §4）。
- [x] `nico-http` 改为依赖 `nico-lb`，`upstream/picker` 删除。

**检查点 B**：`cargo test --workspace` 绿；`cargo tree -d` 无新增重复栈；文件均 ≤500 行。

## 第 3 步：池编译（`nico-server::llm`）

- [x] `chain::compile_pool`：逐成员复用现有物化（`base_url`/凭证/`model_map`/能力），任一失败
      → 整份文档被拒、保 LKG（复用 `LlmBuildError`）。
- [x] 单成员池短路：不构造选择器，走今天的单目标路径（与流量层单成员短路同一做法）。
- [ ] 池成员的排除表状态跨发布按 provider 名继承；文档删除成员即遗忘。（**未做**：排除表是请求内的，见下方说明）
- [x] 发布期拒 `traffic` 角色下的池配置（该角色不含 `nico-llm`）：池路由的 `Target` 是 `Provider`，`NoLlmCompiler` 已拒绝。

**检查点 C**：`cargo test -p nico-server --lib` 绿；单 `provider` 路由的既有 e2e 零改动通过。

## 第 4 步：尝试循环与分类

- [x] 候选集：池成员 ∩ 未在本次请求尝试过 ∩ 能服务本次解析出的物理模型 ∩ 未在排除表冷却中
      ∩ 熔断器闭合。空集且模型过滤是唯一原因 → `403 model_not_allowed`，不进 `fallback`。
- [x] 选择：`PoolStrategy` → `nico-lb` 的 `Picker`。
- [x] 分类先行：`CanonicalError` → {调用方错误, 限流, 凭证, 目标故障}，再决定
      "消耗预算 / 换成员 / 记入排除表 / 报告熔断器"。
- [x] 429 / 401 / 403 不消耗 `retry_budget`；400 / 404 / 422 不换成员并直接作答。
- [x] 429 / 401 / 403 不向熔断器报告（表隔离，design §6）。
- [ ] `fallback` 仅在池耗尽后按既有顺序继续。（**未做 e2e 覆盖**，逻辑已有）

**检查点 D**：`cargo test -p nico-server --test llm_*` 绿；五类失败的断言齐全。

## 第 5 步：可观测性与 e2e

- [x] 指标 `gateway_llm_pool_selected_total` / `gateway_llm_pool_excluded_total`；日志
      `llm.pool` 与 `llm.excluded`。
- [x] e2e：两成员池一个被 429 排除仍全程 200（`llm_pool.rs`）；400 不换成员；排除日志字段。
      模型过滤；熔丝上界。
- [x] 回归：全树 71 个套件绿、`cargo tree -d` 无新增、文件均 ≤500 行。

**检查点 E**：全树绿；`ops_surface` 的指标断言更新。

## 验证命令

```bash
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all --check
cargo tree -d
python3 ./.trellis/scripts/task.py validate 11-11-model-lb
```

## 未落地的两项及其理由

- **跨发布的排除表继承**：现在的排除表是 *请求内* 的（`tried` 集合），因为 M1 的验收只要求"同一请求
  不重复选中"。跨请求的共享冷却表需要一个进程级 store 和按 provider 名寻址的继承规则；它与熔断器
  的表隔离语义要一起定，属于 M2 的范围。当前实现里 429 后**跨请求**仍会重新抽到该成员。
- **fallback 顺序的 e2e**：逻辑已通（`walk_order` 在池之后按 `pooled.max(first)..` 追加），但没有
  专门的端到端断言；单元测试 `pool::*` 覆盖了顺序生成。
