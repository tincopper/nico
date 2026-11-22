# 设计:traffic 路由的同请求 failover

## 1. 不变量与失败模式

1. **响应提交是终点(既有)。** 一个字节到过客户端,请求就再无第二次尝试;`may_replace_attempt`
   的这一款不动。
2. **失败分类是 traffic 自己的。** 连接失败、连接超时、响应头前的上游断连 → walk 改试;upstream
   5xx → 透传(health 照常计数);全请求预算超时 → 不重试。LLM attempt 的分类词
   (`AttemptVerdict`、canonical refusal)不出现在 traffic 侧。
3. **每请求一次池抽取。** 池化路由的 upstream 选择粘住;walk 只在所选 upstream 的成员间移动——
   池级的"哪些 upstream 可用"由候选过滤在抽取时回答,不是逐请求重掷的对象。
4. **预算是成员数,不是重试数。** `retry_budget`(含首次)= 本次请求最多接触的成员数;候选耗尽
   或预算耗尽,请求以既有 502 应答。
5. **健康模型不动。** 同请求内的失败照常计入成员 streak(`member_failed` 的三个计数点不变),
   剔除与冷却的规则一字不改。

失败模式:

| 失败 | 行为 |
|---|---|
| 连接失败 / 连接超时(某成员) | 计入 streak;walk 改试下一候选(预算内) |
| 响应头前上游断连/重置 | 同上(AfterDispatch;重放受 `REPLAY_BODY_LIMIT` 约束) |
| upstream 5xx | 透传给客户端;计入 streak;不重试 |
| 全请求预算超时 | 不重试;既有 deadline 应答 |
| 已试遍候选 / 预算耗尽 | `502 upstream_unreachable` |
| body 超重放上限且失败发生在 dispatch 后 | 不重试;既有失败应答 |

## 2. 契约

- `routes[].retry_budget` 对 upstream 路由开放,含义与 llm 侧一致(含首次的尝试总数,默认 1,
  上限 `MAX_RETRY_BUDGET`)。`check_target_fields` 放开它;池化路由同样可写。
- 无新 YAML 字段;无新指标(尝试记录走既有请求记录的 attempt 事实)。

## 3. 结构

| 件 | 归属 | 内容 |
|---|---|---|
| traffic walk | `nico-http/src/context/`(attempt 旁) | 候选(所选 upstream 的成员 id)、已试集合、剩余预算;`failed()` 裁决、`can_retry()`、最后失败持有 |
| `UpstreamState::select_excluding` | `nico-http/src/upstream/pools.rs` | 在"可用 且 未试"的候选中选;既有 `select` 变为其特例 |
| `select_member` | `proxy/forward.rs` | 有 walk 时从 walk 拿候选;首轮做池抽取并粘住 |
| 门 | `proxy/hooks/{response,llm}.rs` | `may_replace_attempt` 与 `classify_failed` 按路由种类问各自的 walk;`retry_admitted` 结构不动 |

数据面三事实(在线/未提交/可重放)与重放机制(`begin_attempt_body`、`REPLAY_BODY_LIMIT`)是
框架行为,两侧原样共享——这是门的既有形状,不是本次新增。

## 4. 测试策略

| 层 | 覆盖 | 位置 |
|---|---|---|
| walk | 裁决矩阵:连接失败改试/预算耗尽止步/5xx 不改试/5xx 计 streak | `nico-http` 单测 |
| 选择 | tried 排除;池抽取粘住(重试轮不重掷);无候选 → 既有 502 | `pools` 测试 |
| E2E | 双成员其一拒连 → 同请求拿到健康应答;budget=1 失败即答;POST 超重放上限 dispatch 后不重试;5xx 透传 + streak 计数 | `crates/nico-server/tests/http_failover.rs` |
| 不回归 | llm 全部测试;无池/无预算路由行为不变 | 既有套件 |

## 5. 被否决的替代方案

| 方案 | 否决理由 |
|---|---|
| 复用 LLM attempt walk | findings 明令:traffic 的失败分类独立;llm 的 canonical refusal/模型过滤词汇对 upstream 路由无意义 |
| upstream 5xx 也 failover | "上游错误直接透传"是既有立场(nginx `proxy_intercept_errors` 行),且 5xx 的 body 已可能是流式应答;真实需求出现另立任务 |
| 池抽取逐轮重掷 | 一次请求换 upstream 是"分发"语义的破坏;llm 池的"一次抽取粘住"是已被验证的形状 |
| 无预算上限 | 成员数即上限,但 `retry_budget` 显式声明是运维对"最多碰几台"的表态;默认 1 保持今天的形状 |
