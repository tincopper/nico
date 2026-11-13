# LLM 用量归一化与计量

## Goal

让网关**知道自己做成了多少事**：每次成功转换的请求结束时，按 `(route, provider, model)` 记录
provider 自报的 token 数，并暴露为指标。今天的缺口是 usage 只从 provider 解出、重新编码给客户端，
网关自己不保留任何痕迹——一次请求消耗了多少额度，网关答不上来。

这是 roadmap 上"LLM token 限额"的前置：没有稳定的归一口径，限额只能在某一家的会计方式上定义。

## Background

- `crates/nico-llm/src/schema/usage.rs` 已定义完整形状：`Usage { reported, measured }`，
  两个 `UsageCounts`，五种 `TokenKind`（input/output/cache_read/cache_write/reasoning）。
  文件头写明"phase 1 只定义形状"。
- 四个 adapter 各自把 provider 字段映射到 `TokenKind`（`provider/{openai,openai_responses,anthropic,gemini}/usage.rs`），
  未知字段进 `_extra` 而不是丢弃。**归一化的映射已经存在**，缺的是消费。
- `ChatResponse.usage`（`schema/response.rs:213`）是规范值上唯一的携带点；四个客户端 codec 都用
  `usage::encode(response.usage())` 把它写回给调用方。
- 流式响应的 usage 出现在最后一个事件里（OpenAI 的 `include_usage`、Anthropic 的 `message_delta`），
  由各 adapter 的流折叠器汇总。
- `nico-http::HttpContext` 在请求结束时经 `proxy/record.rs` 写一条请求记录并调用
  `metrics::record_request`，这是唯一已有的"每次请求的账"的位置。

## Requirements

### R1 两个口径，不合并

- **reported**：provider 自己报的数。它决定计费，网关无从验证。
- **measured**：网关能自己数的部分。今天只有请求/响应字节数与尝试次数——**不是 token**。
  网关没有 tokenizer，也不应该为此引入一个。
- 两者在记录与指标里**分开呈现**。合并会让"provider 说 1000"与"网关看到 4000 字节"成为同一字段
  的两个来源，而它们回答的是不同问题。

本期的指标只暴露 reported：measured 的字节数已由既有指标覆盖，再记一遍是重复。

### R2 一次请求结束时记录什么

按 `(route_id, provider, model)` 归一的五个 `TokenKind` 的 reported 值，各自独立求和：

- 只对 `convert` 且响应成功的请求记录。失败响应没有 usage，4xx 更不会有。
- 流式与非流式走同一条路径：流式在最后一帧汇总出一个 `Usage`，与非流式的
  `ChatResponse.usage` 语义等价。
- **一次请求可能有多轮尝试**（池成员失败后换人）。只记录**实际服务成功的那一轮**的 usage：
  失败尝试要么没有 usage，要么是废数据。这与 `llm.provider` 记录实际服务方一致。
- model 用**物理模型名**（provider 实际收到的名字），不是调用方写的逻辑名——计费按物理模型。

### R3 指标

- `gateway_llm_tokens_total{route_id, provider, model, kind}`，`kind` ∈ 五个 `TokenKind` 的名字。
- 只递增 reported 值。没有 usage 的响应不产生样本（不发 0：零样本与没有样本是不同事实）。
- 标签基数由配置决定：`route_id`、`provider` 来自文档；`model` 取物化后的物理名。当 provider
  没有 `model_map` 时，请求里的模型名会原样成为标签——这是文档允许的自由输入，接受这个基数，
  并在 README 里写明。

### R4 请求记录

在既有请求记录行里加 `llm.tokens.*` 字段，只写实际报告了的 kind。没有 usage 的记录不写这些字段
（不是写 0）。

### R5 明确不做

- **不做 token 计数**：网关没有 tokenizer，自建计数与 provider 口径必然不同，会让"我们的数"和
  "账单上的数"永远对不上，比不做更糟。
- **不做计费**：没有价格表。R3 的指标是账的原料，不是账单。
- **不做限额**：roadmap 上的下游项。其语义（按天/按月、超限是拒绝还是降级、跨副本如何共享计数）
  每一个都是独立设计。
- **不把 usage 归一成"总 token"**：五种 kind 各自有意义（cache_read 通常不按 input 计价），
  合成总数会丢掉计费所需的信息。
- **不改 `Usage` 的形状**：问题从不是形状。

## Acceptance Criteria

- [ ] 成功的 convert 请求（非流式）记录 provider 报告的那些 kind；未报告的 kind 不出现。
- [ ] 成功的 convert 请求（流式）记录与同一响应非流式形态等价的值。
- [ ] 失败的响应（4xx/5xx）不记录 usage，也不产生 `gateway_llm_tokens_total` 样本。
- [ ] 池路由换了成员后，只记录**实际服务成功**那一轮的 usage（有断言证明失败成员的数不进账）。
- [ ] 指标标签的 `model` 是物理模型名：`model_map: {fast: gpt-x}` 的请求，标签是 `gpt-x`。
- [ ] 标签基数不随请求增长：同一 `(route, provider, model, kind)` 重复请求只增计数。
- [ ] 请求记录字段只在实际有 usage 时出现。
- [ ] 五个基线全绿、全树 ≤500 行、`cargo clippy --workspace --all-targets` 零告警、
      `traffic` 角色仍可编译（不引入 `nico-llm` 依赖）。

## Out of Scope

- token 限额与配额、计费与价格表、网关侧 token 计数（R5）。
- 按 usage 分配流量：`11-11-model-lb` R10 已明确前置是本期口径，但分配本身是另一个任务。
- 异构 provider 的 usage 语义（未知字段进 `_extra`，不解释）。

## Open Questions

（无。R1–R5 的边界由既有代码形状与 roadmap 的排序决定。）
