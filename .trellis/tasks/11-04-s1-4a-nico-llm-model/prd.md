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
- 模块：`schema/`、`protocol/`、`provider/`（含编译后的路由事实 `provider/route.rs`）、`stream/`（共享 SSE framing）、`filter.rs`（路由交换）。逻辑↔物理模型解析属于 `ProviderRoute::model`，不单独建模块。
- `schema/` 只依赖 serde 与基础类型；不依赖 `protocol/`、`provider/` 或 Pingora。`Destination` 只出现在 `provider::Endpoint`（`WireRequest` 的地址部分）。
- 厂商名只出现在 `protocol/<name>` 与 `provider/<name>`；`schema/` 与 `filter.rs`/`filter/` 不得出现 vendor switch（有结构测试）。

### R2：canonical 形状

- 请求：`ChatRequest { model, conversation: Vec<Item>, tools, tool_choice, sampling, response_format, stream, extensions }`；`sampling` 覆盖两端都能表达的旋钮（temperature、top_p、输出上限、stop sequences、seed、presence/frequency penalty）。
- `Item::{Message(Message), ToolCall(ToolCall), ToolResult(ToolResult), Reasoning(Reasoning)}`；`ContentPart::{Text, Image, File}`。
- 响应：`ChatResponse { id, model, choices, usage, extensions }`；`Choice { index, turn: AssistantTurn, finish_reason }`；`AssistantTurn { content, tool_calls, reasoning }`。
- 流：`StreamEvent::{Start, TextDelta, ReasoningDelta, ToolCallStart, ToolCallDelta, ToolCallEnd, Usage, Finish, Error}`；tool call 的三个事件携带该 call 在响应中的稳定 `index`（按位置寻址的协议在 id 出现前只能靠位置标识一个并行 call）；canonical 事件不是 OpenAI chunk DTO。
- 资源与错误：`Usage` 区分 measured 与 provider-reported；`CanonicalError` 携带 `ErrorCategory`、稳定 code、`http_status`、`retryable`、可选 provider code；`RawJson` 只用于调用方声明的 JSON 文档（tool parameters、tool arguments、structured-output schema）与未建模字段，配置/路由路径仍禁止 `serde_json::Value`。
- 每个形状有构造校验与 golden fixture；未知字段进 `extensions`，不得静默丢失。

### R3：OpenAI Chat 客户端 codec

- `protocol/openai_chat.rs` 实现 `ClientCodec`：把客户端 `/v1/chat/completions` 请求（body + 必要头）解码为 canonical；把 canonical response/stream/error 编码回 OpenAI wire。
- 流式：任意字节分片、跨块 UTF-8、多行 `data:`、`[DONE]`、心跳都必须安全；输出事件序列与 canonical 一致。
- 未知字段保留到 `extensions`；已知但 canonical 无法承载的字段（`n > 1`、logprobs、audio 输出、`strict` 等）返回 typed error（422，只报字段名），不静默丢；需要字节级兼容的路由用 `mode: passthrough`。
- 流式编码每响应一个 encoder（不仅 codec 单例）：chunk 需要重复 `Start` 携带的响应 id/模型，终止符在 body 结束时写出。
- 声明 `Capabilities`：能力轴是 streaming / tools / images / files / reasoning / response_format，codec 只声明自己真的映射过的那些（1.4a 的 OpenAI Chat 声明除 `files` 外的五项：file 引用没有 canonical 形态）；发布期记录与 provider 的交集，空交集不拒绝（纯文本不需要能力）；请求体声明的需求超出交集时返回 422 `unsupported_capability`，不静默丢。

### R4：OpenAI 上游 provider 适配

- `provider/openai.rs` 实现 `Provider`：`endpoint(route)` 生成 `Endpoint`（`Destination`、path、headers），`encode(route, request)` 生成 wire body，二者由 `Exchange` 组装为 `WireRequest`；method 不在 `WireRequest` 里：请求行由数据面写出，动词沿用调用方（chat 请求按客户端协议本就是 `POST`，passthrough 必须字节一致）。凭证从 `credential_ref` materialize 为 `Authorization`；`model_map` 把逻辑模型映射为物理模型；`decode_response`/`stream_decoder` 把 OpenAI wire 转 canonical。
- `Provider::native_protocol()` 报出 provider 自己的 wire 协议（OpenAI：`openai.chat`），供发布期判断 `mode: passthrough` 是否合法；`Provider::error_for_status(status)` 只凭状态生成 canonical 错误（不读 body），与 `decode_response` 的失败路径共用同一张状态表——响应头在 body 之前写出，所以失败状态必须能在没有 body 时映射；`Exchange::error_for_status` 暴露同一结果给响应转换。
- provider **不拥有** HTTP/TLS/预算：连接、超时、TLS 策略仍归 `nico-http`。
- provider 4xx/5xx → `CanonicalError`（保留稳定 code 与类别），不把 provider body 原样泄露给客户端；凭证不写日志。
- `Provider` 单例无 per-request 状态；`ProviderStream` 每次响应新建。

### R5：路由与数据面缝

- `routes[].protocol` 只对 `provider` 路由必填（例如 `openai.chat`）；发布期按 codec 注册表校验；未知 protocol/provider kind 发布失败。
- AI 路由（`RouteTarget::Provider`）在路由匹配后、路由请求插件后**有界缓冲**请求体，上限 `request.max_body_bytes`；canonical decode 一次；`WireRequest` 由数据面发往 `Endpoint::destination`。
- convert 路由在发布期构建 `Arc<Exchange>`；`Exchange::prepare` 先取 `endpoint`（只读路由，不依赖 body），再对有界缓冲的 body decode 一次并 encode；`mode: passthrough` 的路由不构建 `Exchange`，数据面按字节直通。
- 两条 front-end 在发布期构造：`ConvertRoute::new(Arc<Exchange>)` 与 `PassthroughRoute::try_new(route, provider)`。后者要求 route 的 mode 是 passthrough、provider kind 与 route 相符、route 的 `protocol` 等于 `provider.native_protocol()`，并编译出 endpoint；不满足即发布失败（`ExchangeBuildError`），所以已发布的 passthrough 路由不会把字节送给一个解析别的协议的 peer。
- 转换路由的请求期拒绝保留**客户端 codec 的错误正文**：`ConvertRoute::prepare` 把 `Exchange::error_reply` 的 `ClientReply`（状态、code、类别、body）装进 `HttpError::LlmRejected`，由数据面唯一 writer 写出；状态与 `x-nico-error-code` 来自 canonical error，正文原样写回，不重新包一层网关 envelope。codec 连自己的错误 envelope 都编不出来时才是 500（网关缺陷）。
- 响应体转换：`nico-http` 定义协议无关的 per-request `ResponseTransform`（`head`/`push`/`finish`，输入/输出字节），`HttpContext` 持有；LLM 路由在响应头/响应体钩子里安装并驱动它。convert 的 transform 对 2xx 按客户端的 stream 标志选择逐块翻译或整块缓冲，对**非 2xx 一律缓冲**（失败文档不是事件流）；缓冲的答复在写头前按声明长度、在 body 阶段按 `max_response_bytes` 计。
- 非流式响应有界缓冲：新增 `request.max_response_bytes`（默认 8 MiB，路由只能收紧）。响应**声明**的长度超限 → 写头前 502 + 稳定 code `upstream_response_too_large`；长度未知（chunked）时状态行已提交，超限只能结束流并记 cancelled，不伪造第二个状态。SSE 永不整块缓冲。
- 插件顺序：LLM 路由允许全局/路由的请求头阶段插件（认证/限流）；body/response 阶段插件在 LLM 路由上 1.4a 不支持（顺序与语义留到后续），日志阶段照常。同一路由同时声明插件 `buffers_body` 与 AI 路由缓冲时发布期拒绝。
- raw 模式：`routes[].mode: passthrough`（客户端协议等于 provider 原生协议）跳过 canonical，字节直通；默认 `convert`。
- **上游请求头的时机**：provider 的 path/headers 必须由路由/provider 配置决定，不能依赖请求体；当前 OpenAI/Anthropic/Responses 的 endpoint 是固定的，满足这一点。Gemini 的 path 含模型名，属于 1.4b 的专门设计项，不在本任务假装已解决。

### R6：可观测与安全

- 日志字段：`llm.protocol`、`llm.provider`、`llm.model`（逻辑/物理）、`llm.stream`、`config.generation`、`http.status`、`elapsed_ms`。
- 不记录 prompt/response body、`Authorization`、provider key；错误响应不复述 provider 内部原因。
- 交换类型的 `Debug` 不打印 header 值、prompt 或响应体：凭证、prompt、body 在日志里没有拼写。
- 发布期每个 protocol/provider 一行 `info`（kind、能力、版本）。

### R7：形状先行的验证

- `schema/` 的 golden JSON fixtures；每个形状的 round-trip/拒绝测试。
- OpenAI codec：请求/响应/流式 decode→encode 的语义 round-trip；未知字段保留。
- provider：用 mock upstream 断言 `Endpoint`/`WireRequest` 的 path/headers/body、`decode_response`/`stream` 的 canonical 事件；`error_for_status` 的每一行与 `decode_response` 的失败路径对同一状态给出相同结果。
- front-end：`ConvertRoute`/`PassthroughRoute` 的 endpoint/protocol/provider/headers、prepare 的 body 与模型映射、请求期拒绝的客户端正文、transform 的 2xx JSON/SSE、provider 429/401、任意分片、`[DONE]`、缓冲上限与 head 失败。
- 端到端：OpenAI client → OpenAI provider（convert 与 passthrough 两种模式）；8 MiB body 分片；SSE 每 200 ms 一个事件；首字节后不重试。
- 结构测试：`schema/`、`protocol.rs`、`provider.rs`、`provider/route.rs`、`filter.rs`、`filter/`、`stream/` 无 vendor 名/switch；`cargo tree -p nico-http | grep nico-llm` 无输出。
- `(client protocol × provider)` 的 matrix harness 在 1.4a 先跑 OpenAI×OpenAI，1.4b 填满其余组合。

## Acceptance Criteria

- [x] `nico-llm` 建立，`schema`/`protocol`/`provider`/`filter` 的边界与依赖方向成立。
- [x] canonical 形状有 fixture/round-trip；未知字段保留；`RawJson` 使用范围受限。
- [x] OpenAI Chat codec 的请求/响应/SSE 语义 round-trip 通过；任意分片/UTF-8 边界安全。
- [x] OpenAI provider 的 prepare/decode/stream 通过 mock；凭证注入、模型映射、错误映射正确。
- [x] `routes[].protocol` 发布期校验；未知 protocol/provider 失败；`mode: passthrough` 字节一致。
- [x] AI 路由有界请求体缓冲（`request.max_body_bytes`）与非流式响应缓冲（`request.max_response_bytes`）：请求体超限 413；非流式响应声明长度超限时写头前 502，长度未知时在头已提交后切流（不伪造第二个状态，见 `pingora-guidelines.md`）。
- [x] 端到端 OpenAI→OpenAI（convert + passthrough）、SSE、错误、日志字段有测试。
- [x] 插件 `buffers_body` 与 AI 路由缓冲冲突在发布期拒绝；LLM 路由的 body/response 插件限制有测试。
- [x] 五条基线全绿；`nico-core` 不新增网络/密码学依赖；`nico-http` 不依赖 `nico-llm`。

## Out of Scope

- Anthropic Messages / OpenAI Responses / Gemini 的 codec/provider（`1.4b`）。
- embeddings / images / audio / video / batches / files。
- usage 归一化/计费、模型 fallback、熔断、分布式限流（阶段 2）。
- TLS（入口与上游）；MCP（`nico-mcp`）。
- 按 `model` 路由与请求体重放（独立设计项）；body/response 阶段插件在 LLM 路由上的顺序语义。

## Deliverables

- `crates/nico-llm`：schema、OpenAI codec/provider、filter/exchange、provider route 的 `model_map`。
- `nico-http`：AI route mode、`ResponseTransform`、AI 路由缓冲与 `request.max_response_bytes`。
- `nico-core`：`routes[].protocol`/`mode`、`RequestBounds::max_response_bytes`、`providers[].model_map`。
- tests：schema fixtures、codec/provider、端到端、结构与依赖检查。
- spec：`directory-structure.md` 的 `nico-llm` 模块树；`pingora-guidelines.md` 的 AI 路由缓冲/转换；`logging-guidelines.md` 的 `llm.*`；`error-handling.md` 的 canonical error 映射。
