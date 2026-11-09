# 阶段 2 可靠性核心：实施计划

> 设计见 `design.md`。顺序是硬约束：Pingora 会话重建取证（第 1 步）通过之前不写尝试循环的代码；
> 配置与发布期编译（第 2 步）是 3/4 的前置；数据面循环（第 5 步）依赖 1 的结论与 4 的判定逻辑。
> 每步保持工作区绿：`cargo +nightly fmt --all -- --check`、`cargo clippy --workspace --all-targets
> --all-features --locked -- -D warnings`、`cargo test --workspace --all-features --locked`。

## 0. 规模

- 新增/修改约 20 个源文件、约 3k 行（含测试），步骤 1–8。
- 依赖链：`1 → {2 → 3 → 4 → 5, 6} → 7 → 8`。第 6 步（role 构建）与 2–5 无依赖，可并行。

## 1. 研究检查点（先做）：Pingora 会话重建取证

- [ ] 回答：在 `proxy` 任务内、**客户端未收到任何字节**之前，能否拆掉当前 upstream 会话、
      按 `AttemptPlan` 换目的地重发请求头与 body？给出可复用的回调组合与最小探针代码。
- [ ] 必须覆盖：换目的地（fallback 目标可能是不同 host/port）时连接池与 `HttpPeer` 的重建；
      请求 body 的二次写出（缓冲 body 是否可从 `HttpContext` 重取）；`fail_to_connect` 与响应头后
      放弃会话各自的发生时机；graceful/取消语义在循环中的表现。
- [ ] 实证不到的（例如某回调组合无法安全重入）一律落成设计修订，不在循环里打补丁。
- [ ] **检查点 R**：`research/pingora-retry.md` 存在，写明选定的挂点、探针证据与风险；设计 §2 的循环
      按结论修订（如需）。

## 2. 配置与发布期编译

- [ ] `nico-core`：schema 三件（`routes[].retry_budget`、`routes[].fallback`、
      `providers[].circuit_breaker`）+ raw/semantic/validate 四段校验 + `document` 测试；
      链长上限与预算上限的常量入文档模型。
- [ ] `nico-server/llm/plan.rs`：路由编译出 `AttemptChain`——逐成员 endpoint/model_map/凭证物化、
      能力交集、目标身份指纹；任一成员 RouteFact → 整条路由发布失败（LKG 保活）。
- [ ] **检查点 A**：发布期拒绝有测试（未知成员、重复成员、坏阈值、成员 RouteFact）；未声明
      fallback/retry 的路由编译结果与现状逐字节一致（零行为变化的证明）。

## 3. 熔断状态机与 SharedServices

- [ ] `nico-llm`：目标级状态机（closed/open/half-open、failure_threshold/cooldown、单探测）；
      状态表按身份指纹继承（配置未变跨 generation 保留）。
- [ ] `nico-server`：`SharedServices` 创建一次、`Arc` 进发布链路与快照；不做通用 Lifecycle Manager。
- [ ] **检查点 B**：状态机全迁移单测 + 继承/重置测试；`SharedServices` 的生命周期（进程启动/关闭）有测试。

## 4. 目标选择与判定（nico-llm）

- [ ] `classify`：provider 状态 → canonical 错误 → {Serve, Attempt, Exhausted}；retryable 沿用闭表。
- [ ] 次序逻辑：预算内重试同一目标 → 计熔断 → 切下一目标 → 链尾 Exhausted；熔断 open 的目标不产生
      AttemptPlan（记 skipped）。
- [ ] **检查点 C**：exchange 级 mock adapter 序列测试（fail→fail→ok、401 即停、发头后不进入判定、
      canonical 只解码一次而逐目标 wire 不同）。

## 5. 数据面尝试循环（nico-http）

- [ ] 字节缝扩展：`AttemptPlan` 的传递与"上一次尝试的结果"问询（保持 `nico-http` 无厂商名、不依赖
      `nico-llm`）；`HttpContext` 的 LLM 状态持有各 attempt 编码（≤ max_body_bytes × 链长，发布期已限）。
- [ ] 尝试循环按第 1 步选定的挂点落地；响应头写出后循环必终止（不变量 2 有断言）。
- [ ] 每尝试日志接插件（`llm.attempt`/`llm.target`/结果/耗时）。
- [ ] **检查点 D**：hook 级测试（无 socket）：循环次序、发头后终止、passthrough 路由零进入。

## 6. traffic/llm role 构建（可与 2–5 并行）

- [ ] `nico-server` feature `llm`（默认开）；`--no-default-features --features traffic` 不编入
      `nico-llm`；`cfg` 门控 llm 模块与注册装配。
- [ ] 角色合法性沿用既有发布校验（空注册表拒绝 provider 路由，LKG 保活）——补一条 e2e 或 plan 测试
      锁定 traffic 形态的行为。
- [ ] **检查点 E**：`cargo check --no-default-features --workspace`、`--features traffic`、
      `--features llm`、`--all-features` 四种组合通过。

## 7. 端到端

- [ ] `nico-server/tests/llm_fallback.rs`：A 502→B 200（缓冲与流式各一）、A 持续失败熔断后 B 服务且
      A 连接数为 0、401 不重试、发头后死亡不换源、重试期间逐尝试日志无 body 无凭证。
- [ ] **检查点 F**：AC 的前四条逐项有 e2e 证据。

## 8. spec 与收口

- [ ] `error-handling.md`：重试/熔断/fallback 的判定表与"发头即单行门"；`pingora-guidelines.md`：
      尝试循环挂点与在途状态；`logging-guidelines.md`：`llm.attempt`/`llm.target`/`llm.circuit`。
- [ ] `config/resources.yaml`：三个新字段的注释与示例（不放需要凭证的活动路由）。
- [ ] **检查点 G（收尾门）**：五条基线 + 四种 feature 组合 + `cargo tree` 依赖边界 + 全树 ≤500 行。

## 复审重点（每步之后）

1. **canonical 只解码一次**：任何路径上出现第二次 body 解码即设计违背。
2. **发头即单行门**：任何"头已写还继续尝试"的路径都是缺陷，不是策略。
3. **nico-http 词汇**：出现 retry/breaker/fallback 语义（而非字节与次序）即越界。
4. **成功路径零变化**：未配置 retry/fallback/breaker 的路由，一次成功请求的字节与阶段 1 逐字节一致。
   失败路径有意变化：内置熔断默认（threshold 5）在连续失败后跳过 endpoint，见 design §5；显式
   `failure_threshold: 0` 才恢复到阶段 1 的"永不跳过"。
5. **文件预算**：出现第 501 行按数据边界拆分。
