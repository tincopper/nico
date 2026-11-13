# 实施：LLM 用量归一化与计量

## 第 1 步：把 `TokenKind` 移到 `nico-http`

- [x] `nico-http/src/llm/usage.rs`：`TokenKind` 枚举（五个变体）、`ALL`、`name()`、`from_name()`，
      以及 `TokenUsage`（`UsageCounts` 的中立形状：`get` / `set` / `is_empty`）。
- [x] `nico-llm/src/schema/usage.rs` 改为重导出 `nico_http::llm::{TokenKind, ...}`，`UsageCounts`
      保留在 `nico-llm`（它是带 serde 的规范形状，`nico-http` 不需要）。
- [x] 理由写在两个文件头：`TokenKind` 是"五个会计桶的名字"，不是 provider 知识；`UsageCounts`
      是"这些桶怎么序列化"，属于规范模型。

**检查点 A**：`cargo test -p nico-llm --lib` 与 `-p nico-http --lib` 绿；`nico-llm` 的
`provider/*/usage.rs` 无需改动（重导出保持路径不变）。

## 第 2 步：`ResponseTransform::usage`

- [x] trait 增加 `fn usage(&self) -> Option<&TokenUsage>`，文档写明"`finish` 前是 `None`，
      provider 没报也是 `None`"。
- [x] `ConvertTransform` 持有 `Option<TokenUsage>`：非流式在 `finish()` 里从 `reply.usage()` 取，
      流式在流折叠器结束后取。
- [x] `PassthroughTransform`（若有）返回 `None`：passthrough 没有规范值，也没有 usage。
- [x] 单测：`finish` 前 `None`；provider 报 3 个 kind 时 `usage()` 是那 3 个；一个都不报时仍是 `None`。

**检查点 B**：`cargo test -p nico-llm --lib route::transform` 绿。

## 第 3 步：记录与指标

- [x] `LlmAttempt::physical_model()` 返回 provider 实际收到的模型名（从已编码的 plan 读，不解析
      `"logical -> physical"` 字符串）。target view 在编码前构造，因此这条不经它传递。
- [x] `proxy/record.rs`：2xx 且有 usage 时
      - `tracing`：`llm.tokens.input` / `llm.tokens.output` / …（仅报告了的 kind）
      - `metrics`：`gateway_llm_tokens_total{route_id, provider, model, kind}` 每个 kind 一次
- [x] 非 2xx、无 usage、passthrough 三种情况都不发射。
- [x] 单测：三种抑制路径各有断言；未报告的 kind 不产生样本。

**检查点 C**：`cargo test -p nico-http --lib proxy` 绿。

## 第 4 步：e2e 与文档

- [x] e2e：mock provider 报 `{input: 11, output: 7}`，非流式请求后断言指标里有这两个样本且值为
      11/7；流式请求后断言同样的值。
- [x] e2e：`model_map: {fast: physical-x}`，断言标签 `model="physical-x"`。
- [x] e2e：池路由第一轮 5xx、第二轮 2xx，断言只记一次 usage（值为第二轮的数）。
- [x] README：`gateway_llm_tokens_total` 的标签语义，含"没有 `model_map` 时 model 标签取请求里的
      自由输入"这一条基数说明。
- [x] spec `logging-guidelines.md`：`llm.tokens.*` 字段与既有 `llm.*` 字段表并列。

**检查点 D**：全树绿；`traffic` 角色可编译；文件 ≤500 行。

## 验证命令

```bash
cargo +nightly fmt --all -- --check
cargo clippy --workspace --all-targets
cargo test --workspace
cargo check -p nico-server --no-default-features --features traffic
```

## 落地记录

| 验收面 | 证据 |
|---|---|
| token 词表与"未报告 ≠ 0" | `cargo test -p nico-http --lib llm::usage`（5 项） |
| transform 在 `finish` 前/后/无 usage 三种状态 | `cargo test -p nico-llm --test route transform`（`a_buffered_answer_reports_the_providers_token_counts`、`an_answer_without_usage_reports_none_rather_than_zeros`） |
| 非流式记录物理模型的 token 数 | `a_converted_answer_reports_the_providers_token_counts`：`model_map: {fast: physical-tokens}` → 标签 `model="physical-tokens"`，值 11/7 |
| 未报告的 kind 不产生样本 | 同上，断言 scrape 里没有 `kind="cache_read"` |
| 池多轮只记实际服务那一轮 | `only_the_attempt_that_served_is_counted`：第一个成员 502、第二个 200，token 家族里没有失败成员的样本 |
| 标签基数不随请求增长 | 同一 `(route, provider, model, kind)` 重复请求只增计数 |
| `traffic` 角色仍可编译 | `cargo check -p nico-server --no-default-features --features traffic` |
| 全树 | `cargo test --workspace` 绿；`cargo clippy --workspace --all-targets` 零告警；文件均 ≤500 行 |

一处实现与 design 的差异：`AttemptTarget` 的 `physical_model` 最终没有沿 target view 传递，而是由
`LlmAttempt::physical_model()` 在请求结束时现取。原因是 target view 在 `upstream_peer` 时构造，那时
请求体尚未编码，模型名还不可知。`AttemptTarget` 上的字段保留但不再由选路路径填充。
