# 阶段 2 可靠性核心：技术设计

## 1. 不变量与失败模式

1. **canonical 仍是唯一的重放单位**。重试与 fallback 都不重放客户端原始字节：`prepare` 解码一次，
   每次尝试由该 canonical 重新编码。缓冲上界仍是 `request.max_body_bytes`，重试不放大内存。
2. **响应提交是单向门**。客户端响应头一旦写出（缓冲答复的翻译头、流式答复的 200/event-stream），
   之后的一切失败只结束当前响应：不重试、不换源。判定点就是现有的 head 翻译点。
3. **决策在 nico-llm，字节在 nico-http**。数据面只问"下一次尝试是什么"并照做；哪些失败算可重试、
   链里下一个目标是谁、熔断是否放行，全部由前端（编译期路由事实 + 请求期 canonical 错误）回答。
   `nico-http` 不出现厂商名，不依赖 `nico-llm`（依赖方向不变：nico-llm → nico-http 的字节缝）。
4. **fallback 是显式的**。操作者声明链；网关不猜测模型等价，不做隐式换源。链成员的能力交集
   是该路由的能力集合，沿用既有能力门（请求需求 ⊆ 交集，否则 422）。
5. **可观测不留空白**。每次尝试记一条结构化日志（目标、结果、耗时；无 body 无凭证）；客户端只见
   最终结果。

失败模式与处置：

| 失败 | 行为 |
|---|---|
| 目标 1 retryable 失败，预算未尽 | 重试同一目标或链内下一目标（见 §3） |
| body 已分派后失败且客户端 raw body > 64 KiB | 不进入重试，按现状翻译错误头应答（重放缓冲 64 KiB 上限，取证 §2.3） |
| 目标失败且不可 retryable（401/403/404） | 立即按 canonical 错误应答，不烧预算；计入熔断 |
| 目标地址无法解析 | 计入该目标的 transport 失败（同 connect 失败）：预算内重试，然后换链内下一目标 |
| 流式发头后 provider 死亡 | 流以错误帧终止；不换源（不变量 2） |
| 链耗尽 | 最后一个 canonical 错误应答客户端 |
| 熔断打开 | 跳过该目标（不建立连接），视同一次"跳过"日志；无目标可服务时按其最近 canonical 错误应答 |
| reload | 在途请求固定在旧 generation 的整条链上（快照语义不变）；新 generation 继承未变目标的熔断状态 |

## 2. 请求路径：从"一次尝试"到"尝试序列"

阶段 1 的路径是 `prepare → 写头 → 写 body → 读响应头 → install_transform`。本设计把它变成
**由 Pingora 内建尝试循环驱动的尝试序列**（取证结论，见 `research/pingora-retry.md` §3.3：
`process_request` 自带 `while retries < max_retries` 循环，每轮重取 peer、重建 upstream 会话、
从下游请求重新克隆请求头并重跑 `upstream_request_filter`；`proxy_to_upstream` 是私有方法，
hook 之外没有合法重入点），判定全部在前端：

```text
prepare (一次, 解码 + 目标0编码)
  └─ AttemptPlan { destination, path, headers, body, streaming }
loop:  Pingora 内建循环，ServerConf::max_retries = 链展开上界，次序控制在 ctx
  upstream_peer: 取当前 AttemptPlan（熔断 open → 跳过记 skipped）→ 建连
  upstream_request_filter: 按当前 attempt 写 path/headers（本轮写头之前，重置 body 累积防回放双计）
  request_body_filter: 已 prepared 的重试轮短路，直接输出 ctx 编码（不二次 decode）
  读响应头 → 非 2xx → 前端 classify(attempt_index, provider_status) ->
       { Serve        }  翻译失败头并应答（现状不变）
       { Attempt(next) }  返回带 retry 位的 Err，循环进入下一轮（先于任何下游写）
       { Exhausted     }  用最后一次的 canonical 错误应答
```

- `AttemptPlan` 是数据面已知的字节级事实（destination/path/headers/body/streaming），与
  `LlmPrepared` 同族；`attempt 0` 就是现在的 `LlmPrepared`。
- 重试决策的单行门是 `session.response_written()`：已写响应头的一律不再重试（不变量 2 由数据面
  断言，框架只防双写头、不防重试污染）；`fail_to_connect`（新增 hook）与 `error_while_proxy`
  是仅有的两个置 retry 位点，`upstream_peer` 自身的 Err 不经过前者，由数据面自行置位。
- **请求体重放走 Pingora 内建 retry buffer（64 KiB 上限，`BODY_BUF_LIMIT` 常量不可配）**。尝试
  循环区分两种失败点：
  1. **body 未分派**（connect/写头失败，或 converting 路由尚未攒完 body 时的失败）：客户端字节
     未消费或仅部分消费，重试轮从 retry buffer 前缀 + 客户端续读重建，任意 body 大小安全；
  2. **body 已分派后失败**（classify 到非 2xx、响应读中断）：重试轮的 body 只能来自 retry
     buffer，因此**仅当客户端 raw body ≤ 64 KiB 安全**——超限时重试轮会发出无 body 的请求
     （buffer 截断后 `request_body_filter` 不再被调用，见取证 §2.3）。
- 因此 `retry_budget`/fallback 对"body 已分派后失败"生效的前提是 **raw body ≤ 64 KiB**；超限时
  非 2xx classify 直接落 `Serve`（按现状翻译错误头应答）。connect 阶段重试不受此限。"大 prompt
  的分派后 fallback"（自管剩余 attempt，自写头与 body、自读响应头）记录为设计债，本里程碑不做。
- 在途状态：`HttpContext` 的 LLM 状态持有 canonical 请求（一份，`max_body_bytes` 上界）与当前
  attempt 的编码结果；每次尝试由 canonical 按该目标重新编码（不变量 1），不为链长复制编码。

## 3. 目标选择：预算、链与熔断的次序

路由的尝试序列 = **重试预算 × fallback 链** 的展开，规则固定：

1. 预算内先用**当前目标**重试（同一目标最多 `retry_budget` 次——网络抖动最可能自愈，且不引入
   换源的不确定性）；
2. 当前目标预算耗尽且仍有失败计数增长 → **熔断计数 +1**，切链内下一目标；
3. 链尾也耗尽 → `Exhausted`。

"换目标"与"重试"分开计数的理由：fallback 是可用性手段，不是重试的放大器——链长不放大对单点
的冲击（每目标受自己的预算约束），熔断统计也因此按目标隔离。

**熔断状态机**（每目标一份，状态在 nico-llm 侧）：

```text
closed --连续失败达 failure_threshold--> open --cooldown 到期--> half-open
half-open --探测成功--> closed        half-open --探测失败--> open
```

- `closed` 放行；`open` 直接跳过（AttemptPlan 不产生，日志记 `skipped`）；`half-open` 只放一个探测请求，
  其余请求同 `open`。**探测结果丢失（客户端断连、请求被取消）不会让目标卡死在 half-open**：一个
  cooldown 后，下一个请求成为新的探测——卡住的上界是操作者自己设的 cooldown，而不是一个计时器。
- 不可重试的失败（401/403/404）不烧预算、立即应答，但同样计入该目标的熔断：持续拒绝的目标正是
  熔断要第一个跳过的。
- 计数按目标身份（provider 名 + base_url + kind + 熔断参数）持久于进程级 `SharedServices`；发布期算出
  每目标的身份指纹，身份未变的 generation 继承状态（与插件限流表同一先例），身份变了——包括操作者
  调优了 `failure_threshold`/`cooldown_ms`——重置：带着旧参数的计数描述的是调优前的那份配置。

## 4. 归属与契约变化

| 归属 | 内容 |
|---|---|
| `nico-core` | schema：`routes[].retry_budget: Option<u8>`、`routes[].fallback: Vec<String>`（名字引用）、
`providers[].circuit_breaker: Option<{failure_threshold, cooldown_ms}>`；语义校验（引用存在、链无重复、
预算 ≤ 上限、阈值/cooldown 范围） |
| `nico-llm` | `Exchange` → 保留单 adapter 契约不变；**新增 `AttemptChain`**：发布期把链上每个成员
编译成 `(adapter, endpoint, model_map)`，请求期 `prepare` 产 canonical + 逐成员 encode；`classify`
（状态 → canonical → retryable/不可）、熔断状态机、`next_attempt` 的次序逻辑。能力交集在链编译时算出 |
| `nico-http` | 挂 Pingora 内建尝试循环（`upstream_peer` 取 attempt、`upstream_request_filter` 作
attempt 边界、`request_body_filter` 对已 prepared 状态短路、`upstream_response_filter` 零字节时点
classify、新增 `fail_to_connect`、`error_while_proxy` 重试单行门）、`AttemptPlan` 在字节缝上的
传递、每尝试日志的接插件；不认识"熔断"与"fallback"这两个词 |
| `nico-server` | `SharedServices`（进程级，持 nico-llm 的熔断状态表，`Arc` 进发布链路）；role feature 与
启动校验装配 |

契约不变式：`Provider`/`ClientCodec` trait 形状不变；`Exchange::prepare` 的签名不变（canonical 语义）；
passthrough 路由**不参与**重试/fallback（它没有 canonical，也没有失败判定——字节转发保持现状）。

## 5. 配置形状

```yaml
routes:
  - id: llm-chat
    provider: primary
    protocol: openai.chat
    # 重试预算：对同一目标的总尝试次数（含首次）。缺省 1 = 现状；上限由文档模型固定。
    retry_budget: 2
    # 显式 fallback 链：按序尝试的 provider 名。缺省空 = 无 fallback。
    fallback: [secondary]
providers:
  - name: primary
    kind: openai
    # 可选熔断参数；缺省给内置默认（threshold 5 / cooldown 30s），显式 0 threshold = 关闭熔断。
    circuit_breaker:
      failure_threshold: 5
      cooldown_ms: 30000
```

发布期校验：链成员必须存在且 kind 可解析；链内无重复；`retry_budget` 落在 `1..=4`；链成员数不超过
上限（`MAX_FALLBACK_MEMBERS = 3`，即主目标之后最多三个）；链上每个成员按自己的 model_map/endpoint 规则
编译（Gemini 的常量 model_map、Anthropic 的输出上限等 RouteFact 全部适用，任一成员拒绝则整条路由发布
失败、LKG 保活）；链能力交集成为路由能力。

**上限是派生出来的，不是两个独立数字。** 一个路由能表达的尝试序列是"预算 × 目标数"，所以数据面
`ServerConf::max_retries` 直接取 `nico_core::MAX_ATTEMPTS_PER_ROUTE`（= `MAX_RETRY_BUDGET` ×
(1 + `MAX_FALLBACK_MEMBERS`)）。框架的循环计数因此不可能小于某个合法文档的序列——否则长链会被框架
截断，客户端拿到的是通用上游错误而不是链尾的 canonical 错误；也不可能大于它，因为文档模型拒绝更长的链。
任何一侧调参都会同时改变另一侧。

**未声明 `circuit_breaker` 的 provider 得到内置默认（threshold 5 / cooldown 30s），这本身是对阶段 1 的
一处行为变化**：一个连续失败五次的 endpoint 从第六个请求起被跳过，请求以该目标最近一次 canonical 错误
应答。这是 R2 想要的语义，不是副作用；"未声明任何字段的路由与阶段 1 逐字节一致"只对**成功路径**和
**单次请求的字节**成立，对"某个 endpoint 已连续失败五次"的路径不成立。显式写 `failure_threshold: 0`
可以恢复阶段 1 的行为。

## 6. traffic/llm role 构建

- `nico-server` 声明两个 feature：`llm`（默认开启，开发形态 = 现状 all-in-one）与 `traffic`（空 feature）。
  可加性意味着 traffic 不删除任何代码路径，只是不编入 `nico-llm`。角色由构建产物决定，bootstrap 不加
  role 字段——运行时撒谎没有意义。
- **一个 traffic 构建里没有注册表**：`nico-llm` 根本没编入，注册表的类型不存在，"空注册表拒绝 provider
  路由"这句话在 traffic 构建里没有对象。发布链路因此只认三个 trait（`LlmCompiler` / `LlmFrontEnds` /
  `LlmCompileError`，见 `reload/llm.rs`）：`llm` 构建给出真实编译器，traffic 构建给出 `NoLlmCompiler`。
  它**拒绝**含 provider 路由的文档（`ErrorCategory::Client`）而不是跳过该路由，于是 LKG 保活，且一个本
  构建服务不了的 route 在发布期就可见，而不是等到第一个请求才发现。
- 产物是 `cargo build -p nico-server --no-default-features --features traffic` 与
  `cargo build -p nico-server --features llm`（包名 `nico-server`，可执行文件名 `nico`）。

## 7. 观测

- 每次尝试一条 `info/warn`：`llm.attempt`（序号）、`llm.target`（provider 名）、结果（`ok`/`failed`/`skipped`）、
  耗时；字段进既有请求记录，不新增请求级字段。
- 熔断状态迁移一条 `warn`（`llm.circuit` = `open`/`half_open`/`closed`，带目标身份）。
- 客户端答复与 `x-nico-error-code` 语义不变（最终结果的 canonical 错误）。

## 8. 测试策略

- 单元：目标选择次序（预算×链）、熔断状态机全迁移、能力交集、身份指纹与 generation 继承。
- exchange 级：mock adapter 序列（fail → fail → ok），断言 canonical 只编码自一次解码、逐目标 wire 不同。
- 数据面：retry 循环的 hook 测试（AttemptPlan 传递、拆会话触发条件、发头后不重试、65 KiB 请求的
  分派后失败不重试——64 KiB 重放边界的 e2e 覆盖）。
- e2e（nico-server）：`llm_fallback.rs`——A 502→B 200 的缓冲与流式各一条；A 持续失败熔断后 B 服务且
  A 连接数为 0；401 不重试；发头后死亡不换源；role：traffic 构建遇到 provider 路由发布失败保 LKG。
- 风险点先取证：Pingora 会话重建（见 implement.md 第 1 步），取证不过则回到设计而不是在循环里打补丁。
