# LLM token 限额：接口 + 进程内配额

## Goal

按**身份**限制 convert 路由消耗的 token：请求前检查预算，成功后按 provider 自报的用量记账，超限
拒绝。roadmap §3 最后一项实质工作。

**交付方式**（用户指令）：接口先行，跨副本共享的外部 KV 是"真实环境"，由进程内 store 作替身——
接口不变，替身先行，多副本部署时换实现。

## Background

- 口径已建立：`gateway_llm_tokens_total` 只记 provider **reported** 的 input/output；
  `record_tokens`（`proxy/record.rs`）是唯一的成功记账点，且能拿到 `HttpContext`。
- 身份已存在：auth 插件（jwt/api_key）经 `ctx.set_identity(subject)` 建立
  `Identity { subject, source }`；`HttpContext::identity()` 可读。
- 进程级状态的归属模式已定：`HttpStateHandle` 持 `UpstreamPools`（跨 reload 存活）——预算 store
  同址。
- 请求级限流的对照：`rate_limit` 插件（令牌桶、LRU 有界表、键取身份）——但那是**请求数**，
  token 是**用量**，且用量在响应后才可知。

## Requirements

### R1 接口先行

- `BudgetStore` trait：`check(key, limit, window, now)` 与 `charge(key, tokens, now)`——预算策略
  （limit/window）由调用方从路由配置传入，store 只管记账与窗口。
- 进程内实现：LRU 有界表（键取 `(route, subject)` 的哈希），固定窗口语义。
- 外部 KV（跨副本共享）是实现该 trait 的**后续任务**；trait 边界即为此而设。

### R2 配置

- `routes[].token_budget: { limit_tokens, window_seconds }`，可选；仅 convert 路由可声明
  （upstream 路由拒绝该字段——它没有 token 可限）。
- 校验：`limit_tokens` `1..=1_000_000_000`；`window_seconds` `1..=31_536_000`（一年）。
- 不新增身份配置：键来自 auth 插件建立的身份。

### R3 检查与记账

- **检查**：`upstream_peer` 选目标之前；身份缺失 → 不限（记录在案：部署方要全覆盖就在路由上放
  auth 插件）。
- **记账**：`record_tokens` 对 2xx 且有 reported usage 的请求，记 `input + output`——两个全价桶。
  cache_read/cache_write/reasoning 不计入（reasoning 已含在 output；cache 是折扣桶，计入会
  高估预算消耗）。
- **事后语义**：用量在响应后才知，所以一次请求可以越过预算；**下一个**请求被拒。这是 LLM
  网关的标准事后限额，文档写明。

### R4 超限回答

- `429`，code `token_budget_exceeded`，`ErrorCategory::Client`，body 说明预算与窗口。

### R5 明确不做

- 跨副本共享（外部 KV 是替身的真实环境，接口已备）。
- 虚拟 key、租户层级、跨路由共享池（`model-lb` R10 排除的 governance）。
- 按 kind 分桶限额、按 provider 差异化限额。
- cache kind 计入预算。

## Acceptance Criteria

- [ ] schema：字段读写、upstream 路由拒绝、范围校验（0 / 超上限 / 窗口 0）。
- [ ] store 单测：首次记账开窗、窗口内累计、过期重置、LRU 有界、超限判定。
- [ ] 检查：有身份且超限 → `429 token_budget_exceeded`；无身份 → 不限（既有行为）；预算未超 → 放行。
- [ ] 记账：2xx + usage → 记 input+output；非 2xx 不记；无身份不记。
- [ ] e2e：api_key 身份 + 小预算 → 第一次成功、第二次 429；断言 provider 收到的请求数。
- [ ] 全树 ≤500 行、clippy 零告警、traffic 角色可编译、五条基线绿。

## Out of Scope

外部 KV 实现、governance（R5）、按 kind 限额、超限后的降级/排队。

## Open Questions

（窗口语义选固定窗口而非滑动/日历对齐：实现最小、可测试，且预算语义下漂移可容忍——一次窗口
边界多算几个 token 不改变"限额存在"这个事实。若部署方需要对齐，是后续参数。）
