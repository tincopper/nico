# nico-llm 协议矩阵：Anthropic / Responses / Gemini

## Goal

在 1.4a 冻结的 canonical 模型、`ClientCodec`、`Provider`、`ProviderStream` 与两条数据面缝上，补齐其余客户端协议与厂商适配，并用 `(客户端协议 × 上游 provider)` 的 golden matrix 证明：**核心没有厂商 switch，转换只发生在两端的适配器**。

客户端协议：OpenAI Chat Completions、OpenAI Responses、Anthropic Messages、Gemini `generateContent`。
上游 provider：OpenAI、Anthropic、Gemini（后续厂商只加 provider 模块）。

## Dependencies

- `1.4a nico-llm：统一模型与 OpenAI 协议`：schema、trait、数据面缝、OpenAI codec/provider、matrix harness。
- 本任务不修改 canonical 形状；若发现形状缺口，先回 1.4a 修正，不在 1.4b 里加“第二套模型”。
- `design.md` / `implement.md` 在 1.4a 的 schema 冻结后编写；本 PRD 是接口与验收契约，`task.py start` 前必须补齐设计/实施产物。

## Requirements

### R1：只加适配器，不改核心

- 新增文件只在 `protocol/<name>.rs` 与 `provider/<name>.rs`（必要时加 `wire/<name>` 私有模块）；`schema/`、`filter.rs`、`stream/sse.rs` 不出现厂商分支。
- 每个新 protocol/provider 在 `builtin_registry()` 注册；未知组合在发布期拒绝。
- 不新增 canonical 的第二套 request/response；Responses 的 input/output items 映射到既有 `Item`/`ChatResponse`。

### R2：Anthropic Messages

- 客户端 codec `anthropic.messages`：请求（system/messages/content blocks/tools/tool_choice/thinking）、响应（content blocks/stop_reason/usage）、SSE（`message_start`/`content_block_start`/`content_block_delta`/`message_delta`/`message_stop`/`ping`/`error`）↔ canonical。
- provider adapter `anthropic`：`/v1/messages`、`x-api-key` + `anthropic-version`、`max_tokens` 映射、thinking/tool blocks；流式事件 → canonical `StreamEvent`。
- 语义边界：Anthropic 的 `system` 是顶层字段，映射到 canonical 的 system item；`tool_use`/`tool_result` 映射到 `Item::ToolCall`/`Item::ToolResult`；thinking 映射到 `Item::Reasoning`（signature 保留在 extensions/字段中）。
- 不支持的能力（例如某些 server tool）在 capability matrix 标记，请求期返回 typed error，不静默丢。

### R3：OpenAI Responses

- 客户端 codec `openai.responses`：`input` items（message/function_call/function_call_output/reasoning）、`tools`、`text.format`、`stream` ↔ canonical；响应 `output` items、`usage`、流式 `response.*` 事件 ↔ canonical。
- provider adapter `openai` 增加原生 `/v1/responses` 路径；对 Anthropic/Gemini 的上游，Responses 请求先经 canonical 再走各自的 chat/stream adapter，**不**为每家实现一套 Responses wire。
- Responses 与 Chat 共享 canonical，不引入 `BifrostResponsesRequest` 式的第二套模型。

### R4：Gemini generateContent

- 客户端 codec `gemini.generate_content`：`contents/parts`、`systemInstruction`、`tools/functionDeclarations`、`generationConfig`、`streamGenerateContent`（SSE）↔ canonical。
- provider adapter `gemini`：`/v1beta/models/{model}:generateContent` 与 `:streamGenerateContent?alt=sse`；`x-goog-api-key`；`functionCall`/`functionResponse`、`finishReason`、`usageMetadata` 映射。
- **模型在 path 的问题**：Gemini endpoint 的 `{model}` 来自物理模型，而客户端 `model` 在 body/URL。设计必须二选一并写清：
  1. 路由/provider 固定物理模型（`model_map` 在该路由上是单值），body model 只做校验；或
  2. 在 upstream head 之前读取 body（pre-body exchange），需要先验证 Pingora 的钩子能否在 `upstream_request_filter` 前拿到完整 body；验证失败则回到方案 1。
  任何方案都不能把“模型在 path、body 后到”这个事实藏起来。

### R5：矩阵与能力

- 支持组合至少覆盖：OpenAI Chat / Anthropic / Responses / Gemini 客户端 × OpenAI / Anthropic / Gemini provider 的 golden fixture（可先做每行的核心组合，但每个组合必须在 capability matrix 有明确 supported/unsupported 结论）。
- 共享 fixture：同一 canonical 对话在四个客户端协议下编码，再由另一客户端协议解码，语义等价；工具调用/结果、图片、reasoning、stop reason、usage 分别有断言。
- 不支持的能力在发布/请求期明确报错；禁止“字段被静默丢弃”的测试通过。
- raw passthrough：客户端协议与 provider 原生协议一致的组合，字节级直通（对照 convert 路径）。

### R6：测试、观测与安全

- SSE：任意字节分片、跨块 UTF-8、多行 data、注释/心跳、`[DONE]`/终止事件；转换后事件顺序与 canonical 一致。
- 错误：各客户端协议的 error envelope；provider body/凭证不出现在响应与日志。
- 日志：`llm.protocol`/`llm.provider`/`llm.model`/`llm.stream` 与 1.4a 一致；新增协议不新增日志字段。
- 结构测试：`schema/`、`filter.rs` 无 vendor 名；`nio-http` 不依赖 `nico-llm`；文件 ≤500 行。
- 五条基线全绿；`(protocol, provider)` matrix 在 CI 可一键运行。

## Acceptance Criteria

- [ ] Anthropic Messages 客户端 ↔ canonical ↔ OpenAI/Anthropic/Gemini provider 的核心组合通过。
- [ ] OpenAI Responses 客户端 ↔ canonical ↔ OpenAI/Anthropic/Gemini provider 的核心组合通过；无第二套 canonical。
- [ ] Gemini generateContent 客户端/provider 通过；模型在 path 的方案已选定并测试。
- [ ] 工具调用/结果、reasoning、图片、stop reason、usage 在矩阵 fixture 中断言；不支持能力显式报错。
- [ ] raw passthrough 与 convert 的字节/语义断言；SSE 分片与背压通过。
- [ ] 错误不泄露 provider body/凭证；日志字段一致。
- [ ] 无 vendor switch 结构测试、依赖边界、文件预算、五条基线全绿。

## Out of Scope

- embeddings / images / audio / video / batches / files / rerank。
- 模型 fallback、熔断、重试预算、usage 计费/归一化（阶段 2）。
- TLS（入口与上游）；MCP（`nico-mcp`）；管理 API。
- 按 `model` 路由/请求体重放（独立设计项）；LLM 路由的 body/response 插件顺序语义。
- 厂商特有的 batch、file、cache、assistant 等管理 API。

## Deliverables

- `protocol/{anthropic,openai_responses,gemini}.rs` 与必要 `wire/` 私有模块。
- `provider/{anthropic,gemini}.rs`，以及 OpenAI provider 的 Responses endpoint。
- 扩展的 capability matrix、共享 fixtures、`(protocol, provider)` matrix 测试。
- `design.md`/`implement.md`（1.4a schema 冻结后补齐）+ spec 更新（`directory-structure`/`pingora-guidelines`/`logging`）。
