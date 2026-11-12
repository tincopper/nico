# 取证：bifrost 如何做模型/后端的负载均衡

> 对象：`/Users/tomgs/workspaces/go_space/bifrost`（Go 网关）。只读取证，按文件位置引用。
> 目的：回答「后端模型列表的负载均衡」在同类产品里是怎么实现的，以及 nico 该怎么切。

## 1. 它有两层池，不是一层

| 层 | 结构 | 证据 |
|---|---|---|
| **provider 池** | `ProviderCandidate { Provider, Weight *float64, KeyIDs }`；`Weight = nil` 表示该候选**不参与负载均衡** | `core/schemas/grant.go:269-284` |
| **key 池** | 每个 provider 下多个 key，各自带 `weight`、`models`（能服务哪些模型）、`use_for_batch_api` 等 | `core/schemas/account.go:143`（`Weight ... // Weight for load balancing between multiple keys`）、`docs/deployment-guides/config-json/providers.mdx:132-155` |

一个请求先由"谁能服务这个 model"过滤出候选（provider × key），再在候选里做**加权随机**。

## 2. 选择算法：加权随机，不是轮询

`core/keyselectors/weightedrandom.go`：`totalWeight` 求和 → `rand.Float64() * totalWeight` → 累加命中。
另有非有限权重的归一化与"全部为 0 时"的兜底。基准测试在 `weightedrandom_bench_test.go`。

轮转只发生在**同一次请求的多次尝试**里：`pickFromRotatingPool`（`core/bifrost.go:7078`）"a weighted
selection with per-cycle exclusion"（`core/bifrost.go:8371`），即一次请求内不重复选中同一个 key。

## 3. 失败如何驱动"轮转"与"排除"（这是关键）

`core/failureclass.go` 把上游错误分成几类，**不同类别的处置不同**：

| 类别 | 例子 | 处置 |
|---|---|---|
| 瞬时上游错误 | `500/502/503/504/529`（529 是 Anthropic 的 overloaded） | **用同一个 key 重试**，不轮转——换凭证对服务器抖动没有帮助 |
| 限流 | 429，或无状态码时匹配 `rate limit` / `tpm exceeded` / `tokens per minute` / `throttled` 等短语 | **轮转**到另一个 key |
| 配额耗尽 | `credit balance` / `spend limit` / `prepayment credits` 等 | **排除**该 key（不再参与选择） |

结论：bifrost 所谓"按 token 语义"**不是**统计 token 用量来分配流量，而是**读取 provider 的
配额/限流信号**，据此轮转或排除。真正决定"分多少流量"的仍然是 `weight`。

## 4. 粘性：session affinity

`core/sessionaffinity.go`：一个带 session id 的请求会绑到上次服务它的 `(provider, model)` 与 key
（`SessionStateKindRoute` / `SessionStateKindKey`），状态存在注入的 `schemas.KVStore` 里。
目的是降低抖动（对缓存友好的模型尤其重要），并减少"同一会话换供应商"带来的质量漂移。

## 5. 有序 fallback 是另一个机制，与池并存

`docs/deployment-guides/config-json/governance.mdx:264-293`：`fallbacks` 是**有序**列表，每项带
`provider` + 可选 `model`；还有 `ttft_timeout_ms`（首 token 超时就切下一个 fallback，最后一个不切）。

即：**同层多个候选**用权重随机，**跨层兜底**用有序链。两者不是同一个东西。

## 6. 与 nico 的现状对照

| 能力 | bifrost | nico 现状 |
|---|---|---|
| 同一 upstream 多实例 | —（它不是反代） | **没有**：`Upstream { scheme, host, port }` 单实例（`crates/nico-core/src/config/upstream.rs`） |
| 同一逻辑模型多家 provider | **有**：provider 池 + 权重 | **没有**：`routes[].provider` 只有一个，`fallback` 是有序链、不是池 |
| 有序 fallback | 有（`fallbacks` + TTFT 超时） | 有（`routes[].fallback`，最多 3 个，发布期物化） |
| 失败排除 | 按错误类别轮转 / 排除 | 有 provider 级熔断器（目标身份 + 冷却 + 半开） |
| 权重随机 | 有 | 没有 |
| 会话粘性 | 有（KVStore） | 没有（无会话概念） |
| token 用量均衡 | **没有**（只有配额信号） | **没有**，且 `usage` 只解码不归一化（`crates/nico-llm/src/schema/usage.rs:1-7`） |

## 7. 对 nico 的三条结论

1. **"基于 token 的负载均衡"要先做 usage 归一化**。nico 现在把 provider 报的 usage 原样保留、不做
   归一化，拿它做分配等于用一家厂商的会计口径去决定另一家的流量。没有归一化之前，token 语义只能
   退化成"读 429 里的 TPM 信号并排除该目标"——那更像熔断，不像均衡。
2. **权重随机比轮询更值得先做**：它是"同一逻辑模型多家 provider"唯一不会随实例数量漂移的分法，
   也是 bifrost 唯一实际使用的分配算法。
3. **失败类别决定处置**这一点 nico 已经有地基（熔断器的"目标身份 + 冷却 + 半开"），缺的是把
   "限流/配额"从"普通失败"里分出来：今天 429 对熔断器就是一次失败，而它其实说明"这家还有救，
   只是该分给别人"。
