# 汇总：bifrost 与 aiway 的模型层负载均衡，以及 nico 的目标形态

> 两份取证分别在 `bifrost-model-load-balancing.md`（本目录）与
> `11-11-upstream-lb` 的取证（`nico-http` 侧只讲流量层）。本文只做汇总与映射，不重复引用细节。

## 1. bifrost 的效果拆成八个能力

| # | 能力 | bifrost 的做法 | aiway 的做法 | nico 今天 |
|---|---|---|---|---|
| 1 | **两层候选池** | provider 候选（带 weight）+ 每 provider 多个 key（带 weight、models） | 每 model 一个 `Vec<Provider>`（带 weight） | 无：`routes[].provider` 单选 |
| 2 | **加权随机** | 每次尝试在候选里加权随机，同请求内排除试过的 | 指数竞速排序，再按序 failover | 无 |
| 3 | **失败分类处置** | 5xx → 同 key 重试；429 → 轮转；配额 → 排除 | **只对传输错误** failover，429/5xx 直接透传 | 熔断器，不分类 |
| 4 | **有序兜底** | 独立于池的 `fallbacks`（provider + 可选 model） | 与分配共用同一个有序列表 | `fallback`（有序，≤3），已物化、能力取交集 |
| 5 | **首 token 截断** | `ttft_timeout_ms`：无首 token 就切下一个；**最后一个不截断** | 无 | 无（只有每读超时与整请求预算） |
| 6 | **模型兼容过滤** | 请求的 model 不匹配 key 的 `models` 就跳过该候选 | 无（provider 没有模型声明） | 发布期 `model_allowlist`，请求期 403 |
| 7 | **会话粘性** | session id → 绑定 `(provider, model)` 与 key，状态在 KVStore | 无 | 无（无会话概念、无外部存储） |
| 8 | **可观测** | 选谁、为何轮转/排除 | 日志打印候选与尝试 | `llm.target` / `llm.attempt` / `llm.outcome` / `llm.circuit` |

另外两条值得记住的事实：

- **两家都没有"按 token 用量分配"**。bifrost 的"token 语义"是读限流/配额**信号**；aiway 的
  `token_usage_config` 只是从响应里按 JSON 路径**提取**用量做日志。真正的 token 均衡在两家都不存在。
- **aiway 的 failover 只看传输错误**（`Client::post` 无 `error_for_status`），429/500 是 `Ok(response)`
  直接透传。这是"有多家 provider 时最该切换的场景"，不能照抄。

## 2. nico 的目标形态（达到 bifrost 的效果，不是照抄它的结构）

### 2.1 两层池压成一层：provider 条目 = `(provider, key)` 对

bifrost 的 key 池解决的是"同一家 provider 有多个 API key"。nico 的 provider 条目已经**一个条目一个
credential**（`credential_ref` 是 provider 级字段），所以"同一家多个 key"在 nico 里就是**多条 provider
条目**（同 kind、同 base_url、不同 credential_ref）。这不需要引入第二个概念，也避免了"key 池"与
"provider 池"两套状态。

代价要说清：nico 因此无法表达"一个 credential 内部的子池"，也不需要——bifrost 的 key 池本质上就是
凭据池。

### 2.2 池与兜底分开（采用 bifrost，放弃 aiway）

nico 已经有 `fallback` 这个有序链，语义清楚。目标形态是**两件事并存**：

- `routes[].providers: [{ name, weight? }]` —— 分发集合，每请求按策略选一个；
- `routes[].fallback: [provider 名]` —— 池里全部不可用后的有序兜底。

aiway 把两者合成"策略排序 + 顺序 failover"更省 schema，但它表达不了"3 个等权 peer + 1 个只兜底的
廉价条目"，而那是实际部署里最常见的形状。bifrost 的形状更值。

### 2.3 失败分类是这次的真正增量

| 失败 | 处置 | 与现有机制的关系 |
|---|---|---|
| 5xx（瞬时） | 同目标重试（`retry_budget`）→ 预算耗尽再换成员 | 熔断器继续计数 |
| 429 限流 | **不重试同目标**，立刻换成员；该成员短冷却 | 不进熔断器的连续失败计数，走独立的排除表 |
| 401/403 凭证/配额 | 换成员；该成员长冷却 | 同上，冷却更长 |
| 400/404/422 调用方错误 | **不换**，直接答 | 现状正确，不能因切换成员掩盖调用方的错 |
| 连接/超时 | 换成员 | 现状：`fallback` 只在预算耗尽后 |

关键约束：**换成员不等于重试同一目标**。今天的 `retry_budget` 与 `fallback` 是"先耗尽预算再换"，
而 429 的正确反应是"立刻换、别浪费预算"。

### 2.4 模型兼容过滤放在请求期，不是发布期

我先前在 prd 里写的"发布期要求每个池成员都能服务该逻辑模型"是错的：那样会把合法的部署挡在发布
之外（例如池里有两个 provider，模型集合只部分重叠，而请求只会用到重叠部分）。bifrost 的做法是
**按请求的 model 过滤候选**。nico 在发布期已经物化了每个 provider 的 `model_map` / `model_allowlist`，
请求期拿到 logical → physical 的解析结果就能判定该成员能否服务，过滤后排不进候选即可。

### 2.5 首 token 截断与会话粘性：分阶段

- **TTFT 截断**属于流式路径的新计时器（今天只有"每读超时"与"整请求预算"）。它是 bifrost 效果的一部分，
  但独立于"选谁"，放第二阶段。
- **会话粘性**需要跨请求、跨进程可共享的存储。nico 现在只有一个本地配置文件源、没有 KVStore。
  单进程内做 LRU 会在多副本部署时变成"每个副本一个粘性"，比不做更糟。放第三阶段，恢复条件是
  "多副本部署 + 出现外部存储"。

### 2.6 一个容易被忘的耦合：尝试预算的乘积

`ServerConf::max_retries` 由 `nico_core::MAX_ATTEMPTS_PER_ROUTE` 决定，而后者是文档自己界定的乘积
（`retry_budget` × 最长链）。引入池之后，**池大小 + 兜底链长度**都进这个乘积，否则熔丝会截断一次本来
合法的尝试序列。spec 明确要求"熔丝来自文档自己的界"，所以这条必须在同一改动里更新。

## 3. 分阶段（每阶段独立可验收）

| 阶段 | 内容 | 达成 bifrost 的哪几项 |
|---|---|---|
| **M1** | 池 + 权重 + `weighted_random`/`round_robin` + 失败分类排除 + 请求期模型过滤 + 观测 + 熔丝乘积 | 1、2、3、6、8 |
| **M2** | `ttft_timeout_ms`（最后一个尝试不截断） | 5 |
| **M3** | 会话粘性（前置：多副本 + 外部 KV） | 7 |

`fallback` 与池分离（第 4 项）在 M1 里成立——nico 已有有序链，M1 只是让它与池并存。

## 4. 不追求的部分（写清理由）

- **短语匹配的错误分类**（`tpm exceeded` 之类）：依赖供应商措辞，nico 的 adapter 已经把上游错误读成
  结构化的 `CanonicalError`，按状态码分类稳定且可测。
- **token 用量均衡**：前置是 usage 归一化（`schema/usage.rs` 现在只定义形状）。在归一化之前做分配，
  等于用一家厂商的会计口径决定另一家的流量。
- **治理层（预算/虚拟 key/租户配额）**：bifrost 的 governance 是另一个产品面，nico 的插件体系与它不
  是同一件事，不在本任务。
