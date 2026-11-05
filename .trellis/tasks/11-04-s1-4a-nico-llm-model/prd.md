# nico-llm：统一模型与 OpenAI 协议

## Goal

建立 `nico-llm` 的**统一数据模型**和第一条完整协议链路：canonical schema、OpenAI Chat Completions 的客户端 codec、OpenAI 上游 provider 适配，以及 AI 路由需要的两条数据面缝（有界请求体缓冲、流式响应转换）。

产品边界是 API 网关 + AI 网关：客户端可能用 OpenAI Chat / OpenAI Responses / Anthropic Messages / Gemini，上游可能是另一家厂商。1.4a 先把**形状**和**一条可工作的 OpenAI 链路**定下来；1.4b 在这个模型上补齐其余客户端协议与厂商。

## 背景

- 1.1 已有 `providers` 段与路由级 `provider` 引用；1.2 已有路由、流式 body、SSE、超时/预算/优雅停机；1.3 已有全局/路由插件。
- `nico-llm` 尚不存在；当前数据面遇到 provider 路由只会在没有 filter 解析时拒绝，不会发起上游请求。
- 现有硬约束：路由选择不读 body；Pingora 读完 body 不会重放。协议转换必须读 body，所以 AI 路由采用**有界缓冲**，canonical 成为可重放的值；非 AI 路由仍然不读 body。
- Bifrost 的分层（canonical schema + 客户端 integration + provider adapter）见 `research/bifrost-protocol-model.md`。

## Dependencies

- `1.1 route-schema`：`providers`、路由级 `provider`、`RouteTarget`。
- `1.2 http-data-plane`：Pingora 钩子、流式 body、SSE、预算、`HttpState`。
- 不依赖 `1.3`；可与 `1.3` 并行。
- `1.4b` 依赖本任务冻结的 schema 与 trait。

## Requirements

### R1：crate 与依赖边界

- 新建 `crates/nico-llm`，依赖 `nico-core` + `nico-http`；**不**被 `nico-http` 反向依赖。
- 模块：`schema/`、`protocol/`、`provider/`、`stream/`（共享 SSE framing）、`filter.rs`（路由交换）、`model.rs`（逻辑↔物理模型）。
- `schema/` 只依赖 serde 与基础类型；不依赖 `protocol/`、`provider/` 或 Pingora。`Destination` 只在 `PreparedRequest` 边界出现。
- 厂商名只出现在 `protocol/<name>` 与 `provider/<name>`；`schema/` 与 `filter.rs` 不得出现 vendor switch（有结构测试）。

### R2：canonical 形状

- 请求：`ChatRequest { model, conversation: Vec<Item>, tools, tool_choice, sampling, response_format, stream, extensions }`。
- `Item::{Message{role, content: Vec<ContentPart>}, ToolCall, ToolResult, Reasoning}`；`ContentPart::{Text, Image, File}`。
- 响应：`ChatResponse { id, model, choices, usage, extensions }`；`Choice { index, turn: AssistantTurn, finish_reason }`；`AssistantTurn { content, tool_calls, reasoning }`。
- 流：`StreamEvent::{Start, TextDelta, ReasoningDelta, ToolCallStart, ToolCallDelta, ToolCallEnd, Usage, Finish, Error}`；canonical 事件不是 OpenAI chunk DTO。
- 资源与错误：`Usage` 区分 measured 与 provider-reported；`CanonicalError` 携带 `ErrorCategory`、稳定 code、`http_status`、`retryable`、可选 provider code；`RawJson` 只用于 tool arguments 和未建模字段，配置/路由路径仍禁止 `serde_json::Value`。
- 每个形状有构造校验与 golden fixture；未知字段进 `extensions`，不得静默丢失。

### R3：OpenAI Chat 客户端 codec

- `protocol/openai_chat.rs` 实现 `ClientCodec`：把客户端 `/v1/chat/completions` 请求（body + 必要头）解码为 canonical；把 canonical response/stream/error 编码回 OpenAI wire。
- 流式：任意字节分片、跨块 UTF-8、多行 `data:`、`[DONE]`、心跳都必须安全；输出事件序列与 canonical 一致。
- 未知字段保留到 `extensions`；不支持的能力（例如工具被路由禁用）返回 typed error，不静默丢。
- 声明 `Capabilities`：streaming / tools / images / files / reasoning / response_format。

### R4：OpenAI 上游 provider 适配

- `provider/openai.rs` 实现 `Provider`：从 canonical 请求生成 `PreparedRequest`（`Destination`、method/path/headers/body）；凭证从 `credential_ref` materialize 为 `Authorization`；`model_map` 把逻辑模型映射为物理模型；`decode_response`/`stream_decoder` 把 OpenAI wire 转 canonical。
- provider **不拥有** HTTP/TLS/预算：连接、超时、TLS 策略仍归 `nico-http`。
- provider 4xx/5xx → `CanonicalError`（保留稳定 code 与类别），不把 provider body 原样泄露给客户端；凭证不写日志。
- `Provider` 单例无 per-request 状态；`ProviderStream` 每次响应新建。

### R5：路由与数据面缝

- `routes[].protocol` 只对 `provider` 路由必填（例如 `openai.chat`）；发布期按 codec 注册表校验；未知 protocol/provider kind 发布失败。
- AI 路由（`RouteTarget::Provider`）在路由匹配后、路由请求插件后**有界缓冲**请求体，上限 `request.max_body_bytes`；canonical decode 一次；`PreparedRequest` 由数据面发往 `Destination`。
- 响应体转换：`nico-http` 定义协议无关的 per-request `ResponseTransform`（`push`/`finish`，输入/输出字节），`HttpContext` 持有；LLM 路由在响应头/响应体钩子里安装并驱动它。
- 非流式响应有界缓冲：新增 `request.max_response_bytes`（默认 8 MiB，路由只能收紧）；超限 → 502 + 稳定 code `upstream_response_too_large`。SSE 永不整块缓冲。
- 插件顺序：LLM 路由允许全局/路由的请求头阶段插件（认证/限流）；body/response 阶段插件在 LLM 路由上 1.4a 不支持（顺序与语义留到后续），日志阶段照常。同一路由同时声明插件 `buffers_body` 与 AI 路由缓冲时发布期拒绝。
- raw 模式：`routes[].mode: passthrough`（客户端协议等于 provider 原生协议）跳过 canonical，字节直通；默认 `convert`。
- **上游请求头的时机**：provider 的 path/headers 必须由路由/provider 配置决定，不能依赖请求体；当前 OpenAI/Anthropic/Responses 的 endpoint 是固定的，满足这一点。Gemini 的 path 含模型名，属于 1.4b 的专门设计项，不在本任务假装已解决。

### R6：可观测与安全

- 日志字段：`llm.protocol`、`llm.provider`、`llm.model`（逻辑/物理）、`llm.stream`、`config.generation`、`http.status`、`elapsed_ms`。
- 不记录 prompt/response body、`Authorization`、provider key；错误响应不复述 provider 内部原因。
- 发布期每个 protocol/provider 一行 `info`（kind、能力、版本）。

### R7：形状先行的验证

- `schema/` 的 golden JSON fixtures；每个形状的 round-trip/拒绝测试。
- OpenAI codec：请求/响应/流式 decode→encode 的语义 round-trip；未知字段保留。
- provider：用 mock upstream 断言 `PreparedRequest` 的 path/headers/body、`decode_response`/`stream` 的 canonical 事件。
- 端到端：OpenAI client → OpenAI provider（convert 与 passthrough 两种模式）；8 MiB body 分片；SSE 每 200 ms 一个事件；首字节后不重试。
- 结构测试：`schema/`、`filter.rs` 无 vendor 名/switch；`cargo tree -p nico-http | grep nico-llm` 无输出。
- `(client protocol × provider)` 的 matrix harness 在 1.4a 先跑 OpenAI×OpenAI，1.4b 填满其余组合。

## Acceptance Criteria

- [ ] `nico-llm` 建立，`schema`/`protocol`/`provider`/`filter` 的边界与依赖方向成立。
- [ ] canonical 形状有 fixture/round-trip；未知字段保留；`RawJson` 使用范围受限。
- [ ] OpenAI Chat codec 的请求/响应/SSE 语义 round-trip 通过；任意分片/UTF-8 边界安全。
- [ ] OpenAI provider 的 prepare/decode/stream 通过 mock；凭证注入、模型映射、错误映射正确。
- [ ] `routes[].protocol` 发布期校验；未知 protocol/provider 失败；`mode: passthrough` 字节一致。
- [ ] AI 路由有界请求体缓冲（`request.max_body_bytes`）与非流式响应缓冲（`request.max_response_bytes`）；超限分别为 413/502。
- [ ] 端到端 OpenAI→OpenAI（convert + passthrough）、SSE、错误、日志字段有测试。
- [ ] 插件 `buffers_body` 与 AI 路由缓冲冲突在发布期拒绝；LLM 路由的 body/response 插件限制有测试。
- [ ] 五条基线全绿；`nico-core` 不新增网络/密码学依赖；`nico-http` 不依赖 `nico-llm`。

## Out of Scope

- Anthropic Messages / OpenAI Responses / Gemini 的 codec/provider（`1.4b`）。
- embeddings / images / audio / video / batches / files。
- usage 归一化/计费、模型 fallback、熔断、分布式限流（阶段 2）。
- TLS（入口与上游）；MCP（`nico-mcp`）。
- 按 `model` 路由与请求体重放（独立设计项）；body/response 阶段插件在 LLM 路由上的顺序语义。

## Deliverables

- `crates/nico-llm`：schema、OpenAI codec/provider、filter/exchange、model map。
- `nico-http`：AI route mode、`ResponseTransform`、AI 路由缓冲与 `request.max_response_bytes`。
- `nico-core`：`routes[].protocol`/`mode`、`RequestBounds::max_response_bytes`、`providers[].model_map`。
- tests：schema fixtures、codec/provider、端到端、结构与依赖检查。
- spec：`directory-structure.md` 的 `nico-llm` 模块树；`pingora-guidelines.md` 的 AI 路由缓冲/转换；`logging-guidelines.md` 的 `llm.*`；`error-handling.md` 的 canonical error 映射。
