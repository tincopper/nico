# 模型层负载均衡：同一逻辑模型的多 provider 池（对标 bifrost）

## Goal

让一个逻辑模型对应多家 provider：按权重分发、按失败类别轮转或排除、耗尽后走有序兜底。效果对标
bifrost 的 provider/key 池（见 `research/bifrost-model-load-balancing.md` 与
`research/synthesis-bifrost-vs-aiway.md`），但结构上适配 nico 的既有接缝，不照抄它的两套池。

今天的空缺：`routes[].provider` 只能指向一个 provider；`fallback` 是有序链，不是分发集合；
熔断器把所有失败同等计数，因此一个 429 与一次连接失败对它的含义相同。

## Background

- 发布期物化：一条 provider 路由在发布时解析 `base_url`、`model_map`、凭证与能力，`fallback` 成员
  逐个物化且能力取交集（`crates/nico-server/src/llm/`）。池必须复用这条链路，否则会出现"发布成功但
  某个成员其实不可服务"。
- `ServerConf::max_retries` = `nico_core::MAX_ATTEMPTS_PER_ROUTE`，后者是文档自己界定的乘积；spec 明确
  要求熔丝与文档界一致，引入池会改变这个乘积。
- 失败分类的原料已经存在：`CanonicalError` 带稳定 code、状态与 `retryable`；provider 状态到 canonical
  的映射是一张固定表（`.trellis/spec/backend/error-handling.md`）。
- 每个 provider 的 `model_allowlist` / `model_map` 已在发布期物化，因此"这个成员能不能服务请求的
  模型"在请求期是一个查表动作，不需要额外 I/O。
- usage 尚未归一化（`crates/nico-llm/src/schema/usage.rs:1-7`），所以"按 token 分配"没有可比口径。

## Requirements

### R1 池配置

```yaml
routes:
  - id: llm-chat
    path_prefix: /v1/chat/
    protocol: openai.chat
    providers:                     # 池：每请求按策略选一个成员
      - name: openai-primary
        weight: 3
      - name: openai-secondary     # weight 缺省 = 1
    strategy: weighted_random      # weighted_random（默认）| round_robin
    fallback: [anthropic-primary]  # 池耗尽后的有序兜底，语义不变
```

- `providers` 与单个 `provider` **互斥**，两者都写或都不写都是发布期拒绝。
- 成员必须是已声明的 provider 名；`weight` 范围 `0..=1000`，缺省 `1`。
- `weight: 0` 表示**不参与分发**，只在显式指名或作为兜底时被使用（等价于 bifrost 的
  `Weight = nil` "not a load-balancing candidate"）。
- 池上限由 `schema_bounds` 新增一个有界字段约束（与 routes/upstreams 同样的做法）。

### R2 选择

- `weighted_random`（默认）：按成员权重做加权随机——bifrost 唯一实际使用的分配算法，也是
  "成员数量变化时不漂移"的唯一分法。
- `round_robin`：按池顺序轮转。
- **同一次请求内不重复选中**：每次尝试从"未尝试过的成员"里选；池被试完才进入 `fallback`。
- 选择不引入随机数依赖：随机源通过一个可注入的 trait/闭包传入，单测用确定性序列，生产用
  `fastrand` 之类的现有能力（若仓库没有随机依赖，则用进程内单调计数器派生的伪随机，不新增 crate）。

### R3 失败分类决定处置

| 失败 | 识别 | 处置 |
|---|---|---|
| 本地连接/超时失败 | 现有 `FailureStage` | 同目标重试（`retry_budget`）→ 预算耗尽换成员 |
| 5xx | `CanonicalError` 的 `retryable` | 同上；熔断器照常计数 |
| 429 限流 | 状态 429（映射到 `upstream_rate_limited`） | **不消耗 `retry_budget`**，立刻换成员；该成员进入短冷却 |
| 401 / 403 凭证或配额 | 状态 401/403 | 换成员；该成员进入长冷却 |
| 400 / 404 / 422 调用方错误 | 以上都不是 | **不换成员**，直接作答 |

- 短冷却默认 `10s`、长冷却默认 `300s`，两个值都写成常量（不进文档 schema），因为它们是"失败类别"
  的属性而不是部署参数；一个需要调的部署问题应该改熔断器的 `cooldown_ms`。
- 排除表与熔断器**并列而非合并**：熔断器管"这个目标整体不健康"，排除表管"这个成员现在不该被分到
  流量"。两者的身份（目标身份 vs 池成员）与冷却长度都不同，合并会让 429 把一条 provider 的熔断器
  打满 300s。

### R4 池与兜底并存

- `fallback` 语义不变：池里全部成员被尝试或都不可服务后，按 `fallback` 顺序继续。
- 发布期要求与今天一致：`fallback` 每个成员必须可服务，否则整份文档被拒、保 LKG。
- 池成员不再要求"发布期能力交集"（先前 prd 的写法会挡掉合法部署）；改为请求期过滤，见 R5。

### R5 请求期模型过滤

- 选定成员之前先过滤：该成员能否服务本次请求解析出的物理模型（`model_map` / `model_allowlist`
  已物化，是查表）。
- 过滤后候选为空 → 不进 `fallback`，直接按现有规则回答 `403 model_not_allowed`（这是"路由不服务该
  模型"，不是"成员都挂了"）。
- 这条把 bifrost 的 `keys.models` 过滤落到 nico 已有的物化结果上，不新增配置面。

### R6 尝试预算的乘积

- `MAX_ATTEMPTS_PER_ROUTE` 必须把**池大小**与 `fallback` 长度一起算进乘积，否则熔丝会截断一次本来
  合法的尝试序列（spec 已规定熔丝来自文档自己的界）。
- `retry_budget` 的语义不变：它是"单个成员吸收的尝试次数"，不是"整条路由的次数"。
- 被 429/401 排除的尝试**不消耗** `retry_budget`（R3），因此总尝试数上界仍是"预算 × 成员数"的有限值。

### R7 可观测性

- 日志：复用 `llm.target`（成员名）、`llm.attempt`、`llm.outcome`；新增 `llm.pool`（池名 =
  路由 id）与 `llm.excluded`（排除原因：`rate_limited` / `credential` / `circuit_open`）。
- 指标：`gateway_llm_pool_selected_total{route_id, provider}` 与
  `gateway_llm_pool_excluded_total{route_id, provider, reason}`；标签取文档里的名字，基数可控。
- 选中的成员在请求记录里仍是 `llm.provider`（与实际服务方一致），池只加"为什么不是别人"。

### R8（M2）首 token 截断

- `routes[].ttft_timeout_ms`（可选，`1..=300_000`）：该路由的一次尝试等待响应时，单次读不得超过
  这个预算；超时即视为该成员失败并切下一个成员。
- **语义边界（实现约束，非设计偏好）**：Pingora 的 `read_timeout` 在会话建立时设定、作用于每一次
  读，且 `upstream_peer` 在请求体解码之前运行——因此无法实现"只在首个字节前生效"。该字段的语义是
  **"这个路由的一次读最多等多久"**，流式与非流式同样适用。一个在预算内作答的缓冲请求不受影响；
  一个比预算更慢的缓冲请求会被它截断，这是该字段的代价而非缺陷。
- `upstream_peer` 早于 body 解码，所以"是否流式"在首轮不可知；不做预判（预判会误伤非流式请求），
  按路由声明统一生效。

### R9（M3）会话粘性

- 带会话标识的请求绑定到上次服务它的 `(provider, model)`，降低抖动。
- **前置条件**：多副本部署 + 一个可共享的外部存储。nico 今天没有 KVStore，单进程 LRU 会在多副本下
  变成"每个副本各自粘性"，比不做更糟。因此本阶段只留门，不实现。

### R10 明确不做

- **按 token 用量分配**：前置是 usage 归一化（`schema/usage.rs` 只定义形状）。在归一化之前做分配，
  等于用一家厂商的会计口径决定另一家的流量。
- **按错误文本短语分类**（`tpm exceeded` 等）：依赖供应商措辞；`CanonicalError` 已提供稳定依据。
- **治理层**（虚拟 key、租户预算、跨成员配额池）：bifrost 的 governance 是另一个产品面。

## Out of Scope

- 上游地址层（同一 upstream 多实例）——那是 `11-11-upstream-lb`。
- 会话粘性（R9 只留门）、TTFT 之外的流式超时、按响应延迟动态调权。
- 一致性哈希、区域感知、按请求体内容选择。
- 每个成员独立的超时/重试配置（成员级差异继续用"多条 provider 条目"表达）。

## Acceptance Criteria

M1（本任务的主体）：

- [ ] `providers` 与 `provider` 同时出现/都缺失都被拒，字段名正确；成员未声明也被拒；顶层仍是
      `deny_unknown_fields`。
- [ ] 发布期：池成员的 `base_url`/凭证/能力在发布时物化，任一成员不可物化 → 整份文档被拒、保 LKG。
- [ ] `weighted_random` 的分布单测用注入的确定性随机源：权重 `3:1` 的样本比例落在容差内；
      `weight: 0` 的成员永不被分发选中。
- [ ] `round_robin` 在池内轮转；同请求内不重复选中；池试完才进 `fallback`。
- [ ] 失败分类：429 不消耗 `retry_budget` 且立刻换成员；401/403 换成员且长冷却；5xx 走预算 + 熔断；
      400/404/422 **不换成员**（有断言证明上游只被调用一次）。
- [ ] 排除表与熔断器互不污染：一个成员被 429 排除后，它的熔断器连续失败计数不增加（或增加的证据不足以
      触发熔断，二选一在 design 里定并写测试）。
- [ ] 请求期模型过滤：池里只有部分成员能服务请求的模型时，其余成员不被选中；全部不能服务时回答
      `403 model_not_allowed` 而不进 `fallback`。
- [ ] 熔丝：池 3 成员 + `retry_budget: 2` + `fallback` 2 成员时，`MAX_ATTEMPTS_PER_ROUTE` 覆盖全部
      尝试，且没有路由能越过它。
- [ ] 观测：选中/排除的指标与日志字段按 R7 出现；标签基数不随请求增长。
- [ ] 五条基线全绿、角色矩阵两个组合都能编译（`traffic` 角色不含 `nico-llm`，池配置在发布期被拒）、
      全树 ≤500 行、`cargo tree -d` 无新增重复栈。

M2 / M3（分阶段，各自有独立验收）：

- [ ] M2：`ttft_timeout_ms` 在首 token 超时后切下一个成员；最后一个尝试不截断；非流式不受影响。
- [ ] M3：会话粘性（前置：多副本 + 外部 KV；未满足前不实现）。

## Open Questions

（已定：`weight: 0` = 不参与分发、仅在显式指名或兜底时使用；M2（TTFT）与 M1 分开；排除表与熔断器
并列，429 不污染 provider 熔断器的连续失败计数。）

（无其它待决项。设计文档在流量层落地之后写——模型层的选择器应复用流量层将建立的
"候选 + 状态 + 剔除"原语，提前写会要么重复它、要么先占掉它的形状。）
