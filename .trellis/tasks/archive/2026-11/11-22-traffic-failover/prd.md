# traffic 路由的同请求 failover(G2)

## Goal

普通 traffic 路由在一次请求内,对"连接失败/连接超时/响应头前断连"这类成员故障,自动改试下一个
候选成员——不再把故障留给被动剔除去改善**后续**请求。响应一旦提交、body 不可重放、或预算耗尽,
请求按既有路径失败;失败分类是 traffic 自己的,不套用 LLM attempt 的词汇。

## Background

重试的**门**已经存在且与路由种类无关:`fail_to_connect` / `error_while_proxy` 都经过
`retry_admitted`(数据面三事实 + walk 裁决),失败计数也在三个点各司其职。缺的是 **walk**:
`classify_failed` 只问 `ctx.llm_mut()` 的 attempt,traffic 路由没有 walk → 裁决恒否 →
`may_replace_attempt` 的最后一款(走是否还有一次)恒假。同时 `select_member` 每轮自由重选,
没有"本请求已试过谁"的记忆;对池化路由,池的抽取会每轮重掷(轮转位置推进),而不是"一次抽取
粘住"。

nginx 对应能力:`proxy_next_upstream error timeout`(默认,不含非幂等);nico 的立场来自
审查报告 G2:独立失败分类、幂等性、body 重放能力、次数/时间预算、响应提交后禁止重试;保留
既有健康模型(被动剔除照常计数,同一请求内的失败同样计入 streak)。

## Requirements

### R1|schema:upstream 路由的 `retry_budget`

- `routes[].retry_budget` 对 upstream 路由开放(此前被 `check_target_fields` 拒绝):
  含首次在内,本次请求最多尝试的成员数;语义与 llm 侧一致("one target may absorb, including
  the first"),上限沿用 `MAX_RETRY_BUDGET`。
- 无该字段的路由 = 1(现行为:一次尝试,失败即答)。

### R2|traffic walk

- `HttpContext` 增加 traffic 路由的尝试状态:候选(所选 upstream 的成员,减去已试)、已试集合、
  剩余预算。池化路由的**池抽取每请求一次**并粘住;walk 只在该 upstream 的成员间移动。
- 每轮 `select_member` 在"可用 且 未试"的候选中选;无候选 → 请求以既有
  `502 upstream_unreachable` 失败。
- walk 的裁决是 traffic 自己的:连接失败、连接超时、响应头到达前的上游断连/重置 → 改试下一位;
  upstream 5xx 应答**原样透传**,不计入同请求 failover(与"上游错误直接透传"的既有立场一致),
  但照常计入成员健康 streak;全请求预算超时不是成员的错,不重试。

### R3|门与重放

- `may_replace_attempt` / `classify_failed` / `replace_attempt_failure_with_last` 扩展为
  两侧都能回答:数据面三事实原样(重放能力对 traffic 同样成立——Pingora 的重放缓冲与
  `REPLAY_BODY_LIMIT` 是框架行为,`begin_attempt_body` 每轮已复位);walk 问句按路由种类分流。
- 响应已提交(含流式已开始)永不重试:既有事实,不变。

### R4|范围外

- upstream 5xx 的同请求 failover(透传立场);llm attempt 语义的任何改动;跨 upstream 的
  成员级 failover(池级已有可用性收敛,选中的 upstream 全灭即失败);请求体缓冲策略的改动;
  health 模型的任何迁移。

## Acceptance Criteria

- [ ] AC1:单 upstream 双成员,其一拒绝连接 → 同一请求拿到健康成员的应答;budget=1 时不 failover,
      失败即答。
- [ ] AC2:POST(body 超重放上限)在 dispatch 后失败 → 不重试,按既有失败应答;连接失败发生在
      dispatch 前 → 仍 failover。
- [ ] AC3:canary 路由(池)粘住抽中的 upstream,成员级 failover 只在所选 upstream 内走;
      上游 5xx 透传且计入健康 streak。
- [ ] AC4:全灭(预算耗尽/无候选)→ `502 upstream_unreachable`;被试成员照常计入剔除 streak;
      成功的失败-重试有"attempt failed"级别的可观察记录。
- [ ] AC5:门禁全绿(fmt/clippy/test/doc/deny/预算/matrix),llm 侧全部测试不回归。

## Notes

- 与 llm attempt 的关系是**镜像而非复用**:门(`retry_admitted`)与重放机制(`begin_attempt_body`)
  已共享;walk 的状态与裁决是 traffic 自己的。findings 的验收矩阵(连接失败/非幂等/超重放/
  已提交/全坏恢复)逐条入测试。
