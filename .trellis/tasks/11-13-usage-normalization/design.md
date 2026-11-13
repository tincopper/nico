# 设计：LLM 用量归一化与计量

## 1. 不变量与失败模式

| 不变量 | 违反时的失败模式 |
|---|---|
| **I1 只记实际服务成功的那一轮。** 一次请求的多轮尝试里，只有产生客户端答案的那一轮有 usage。 | 失败成员的废数据进入账里，一个请求被记成 N 份用量。 |
| **I2 usage 在响应体结束时定稿。** 流式的 usage 在最后一帧，非流式在完整 body 解出后；两者都在同一个收敛点。 | 在响应头处读取，得到空 usage；或在流中途读取，拿到半份。 |
| **I3 两个口径分开。** reported（provider 说）与 measured（网关数的字节）不合并。 | "provider 报了 1000 token" 与 "网关看到 4000 字节" 变成同一字段的两个来源，读者无法判断哪句是实话。 |
| **I4 标签只来自文档。** `route_id`、`provider`、`model` 都是发布期确定的名字。 | 标签基数随流量增长，指标后端被一个用户的模型名打爆。 |
| **I5 没有 usage 就不产生样本。** 没报告 ≠ 报了 0。 | 均值类查询被大量虚假零值污染。 |
| **I6 不引入 tokenizer。** 网关从不自己数 token。 | 自建计数与 provider 口径必然不同，"我们的数"和"账单上的数"永远对不上，比不做更糟。 |

## 2. usage 从哪里来

`ResponseTransform` 是唯一的收敛点：无论是缓冲响应还是流式响应，`finish()` 都在响应体结束时被调用一次。

```
provider body ──► ConvertTransform::push ──► 客户端字节
                        │
                  finish()  ← 非流式：reply.usage()；流式：流折叠器的最后一帧
                        │
                        ▼
                 Usage { reported, measured }
```

改动：`ResponseTransform` 增加一个 `usage()` 访问器，在 `finish()` 之后可读。数据面在写完响应后取它。

```rust
pub trait ResponseTransform: Send {
    fn head(...) -> Result<LlmResponseHead, HttpError>;
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<u8>, HttpError>;
    fn finish(&mut self) -> Result<Vec<u8>, HttpError>;

    /// Returns the usage the exchange reported, once the body is complete.
    ///
    /// `None` before `finish` and for a provider that reported nothing: "not yet known" and
    /// "reported zero" are different facts and the caller must not conflate them.
    fn usage(&self) -> Option<&Usage>;
}
```

`Usage` 是 `nico-llm` 的类型，而 trait 在 `nico-http`。依赖方向不允许 `nico-http` 认识它，所以 trait 返回一个
**本 crate 的中立形状**：

```rust
// nico-http::llm
/// Provider-reported token counts, by canonical kind.
///
/// The five kinds are a closed list because the framing is: a provider reports tokens it billed,
/// and these are the buckets a bill is made of. `nico-http` holds this shape without knowing any
/// provider — a front-end maps its own usage onto it.
pub struct TokenUsage { /* [(TokenKind, u64); N] */ }
pub enum TokenKind { Input, Output, CacheRead, CacheWrite, Reasoning }
```

`TokenKind` 已在 `nico-llm::schema` 定义。**不把它提到 `nico-http`**：那会让数据平面拥有一个
provider 会计概念，而它只需要"五个数字和一个名字"。但重复定义又不划算——两个同名枚举必然漂移。

**决定：把 `TokenKind` 与它的 name 表移到 `nico-http`，`nico-llm` 重导出。** 理由是方向：
`nico-llm` 已经依赖 `nico-http`，移动不改变依赖图；而 `TokenKind` 的九个消费者里，有六个在
`nico-llm` 的 provider 侧，它们本来就经由 `nico-llm` 的 schema 引用它。

## 3. 记录发生在哪

`proxy/record.rs` 是唯一的请求记录点，也是唯一的指标发射点。它已经拿到 `HttpContext`，因此：

- 成功判定复用既有逻辑（`http.status` 在 2xx 内）。
- usage 从 `ctx.llm().transform().usage()` 读取。transform 在请求结束前已被 `finish()`。
- `llm.provider` 记录的是实际服务方（池的既有语义），因此 `(route, provider)` 天然是"谁服务的"。

```
请求结束 ──► record::write
                ├── tracing: llm.tokens.* （仅报告了的 kind）
                └── metrics: gateway_llm_tokens_total{route_id, provider, model, kind}
```

## 4. 指标

```rust
metrics::counter!(
    "gateway_llm_tokens_total",
    "route_id" => route_id,
    "provider" => provider,   // 实际服务方，与 llm.provider 同源
    "model"    => physical,   // provider 收到的名字
    "kind"     => kind_name,  // input / output / cache_read / cache_write / reasoning
).increment(count);
```

- 每个报告了的 kind 发一次。未报告的 kind 不发样本（I5）。
- `physical` 从 `AttemptTarget.model()` 的模式标签取——它已经格式化为 `logical -> physical`。
  需要一个只取 physical 的访问器，而不是解析那个字符串。
- **基数说明**：`model` 是文档 `model_map` 的**值**，因此基数 = provider 的 `model_map` 值域。
  当 provider 没有 `model_map` 且调用方写入任意模型名时，该名字会成为标签——这是文档允许的自由
  输入。README 必须写明这一条，因为它是唯一一个能随流量增长标签的地方。

## 5. 池与多轮尝试

`AttemptSequence` 的多轮尝试里，只有产生客户端答案的那一轮的 transform 被 `finish()`。
失败轮次的 transform 从未安装到 `HttpContext`（`install_transform` 在响应头到达时执行），
因此 `record.rs` 读到的必然是最后一轮的。

这满足 I1，且不需要额外状态：**"唯一安装的 transform"就是"实际服务的那一轮"**。

## 6. 备选方案与取舍

| 备选 | 为什么不做 |
|---|---|
| **在 `AttemptSequence` 里累计 usage** | 序列不知道哪一轮成功；它只有终态的 verdict。请求记录点已经在正确的位置，绕过去会造出第二个账本。 |
| **网关自己数 token** | I6。 |
| **把 usage 归一成一个总数** | 五种 kind 的计价不同（cache_read 通常不计 input 价），合成总数丢掉计费信息。 |
| **指标里同时暴露 measured** | measured 是字节数，既有指标已覆盖；再记一遍是重复，且会让两张表的口径漂移。 |
| **在 `nico-http` 重新定义一份 `TokenKind`** | 两个同名枚举必然漂移。移动共享定义更便宜，依赖方向也允许。 |
| **用 `llm.provider` 之外的标签表达"哪一轮"** | 会增加一个基数因子而不增加信息：账只关心实际服务方。 |

## 7. 兼容性与回滚

- 文档兼容：本期不新增任何配置字段。
- 行为兼容：没有 usage 的响应（老 provider、部分流式）只是不产生样本，不改变任何响应字节。
- 指标兼容：新增一个 family，既有家族不变。
- 回滚形状：删掉 `record.rs` 的两个发射点、`ResponseTransform::usage`、`AttemptTarget` 的
  physical 访问器。无数据迁移。

## 8. 测试策略

| 层 | 内容 |
|---|---|
| 契约单测 | `ResponseTransform::usage` 在 `finish` 前为 `None`、`finish` 后为报告值；provider 没报时仍为 `None`（不是零） |
| 映射单测 | 四个 adapter 的 usage 解码与既有的 `schema/usage` 测试一致（已存在，作为回归基线） |
| 记录单测 | 2xx 产生 `llm.tokens.*` 与指标样本；4xx 不产生；未报告的 kind 不产生 |
| 池单测 | 两轮尝试（第一轮 5xx、第二轮 2xx）只记一次 usage |
| 指标单测 | 同一 `(route, provider, model, kind)` 重复请求只增计数；`model_map` 存在时标签是物理名 |
| e2e | 非流式与流式各一次，断言指标里的 token 数与 mock provider 报的一致 |
