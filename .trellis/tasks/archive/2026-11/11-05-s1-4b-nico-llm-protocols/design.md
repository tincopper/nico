# nico-llm 协议矩阵：技术设计

> 需求见 `prd.md`。1.4a 冻结的 canonical 形状、`ClientCodec`、`Provider`、`ProviderStream` 与两条数据面缝是本设计的前提；1.4b 只增加适配器，并补上**三个必须在发布期确定的路由事实**。

## 1. 不变量与本次边界

1. **两条轴互不推导**：客户端协议（`openai.chat` / `openai.responses` / `anthropic.messages` / `gemini.generate_content`）与 provider 方言（`openai` / `openai_responses` / `anthropic` / `gemini`）各自独立，canonical 是唯一公共词汇。翻译路径数仍是 *clients + providers*。
2. **`schema/` 不动**：1.4b 不新增 canonical 形状、不改 `Sampling`/`Item`/`StreamEvent`/`ChatResponse`。`top_k`、thinking budget、Responses 的 reasoning 句柄等承载不了的东西按 1.4a 的三分表（modelled / preserved / refused）在 codec 里**显式拒绝**，不回写 canonical。
3. **`filter.rs`、`route/`、`schema/` 仍无厂商名**：新逻辑只落 `protocol/<vendor>.rs`、`provider/<vendor>.rs`、`wire/`（framing/编码助手）与厂商无关的共享解析（`provider/base_url.rs`）；`filter.rs` 只多一步厂商无关的钉模型比对。
4. **上游 head 在请求 body 之前写出**（阶段 0 实证约束，`nico-http` 契约）：`LlmRoute::path()` 是发布期值，`Provider::endpoint()` 只读路由事实。任何“读了 body 才知道 path”的方案都要先推翻这条契约，因此本设计把它当作硬边界（§2.1）。
5. **失败闭集**：不可服务的组合在发布期拒绝（保留 LKG）；运行期缺能力返回 typed 422；provider 的 body/凭证不进响应与日志。1.4a 的状态表、`error_for_status` 契约、`CanonicalError` 归属全部沿用。

本次唯一动到的 1.4a 契约是三处加法，且都在发布期生效：

| 扩展 | 位置 | 由谁决定 |
|---|---|---|
| `Endpoint::with_model` / `Endpoint::model` | `provider/endpoint.rs` | 端点 path 里已经写进去的物理模型 |
| `ProviderRoute::with_max_output_tokens` / `max_output_tokens` | `provider/route.rs` | 端点要求的输出上限缺省（`NonZeroU32`，零不可表达） |
| `EndpointError::RouteFact { field, reason }` | `provider/endpoint.rs` | 适配器拒绝一组它无法据以编码的路由事实 |

`ClientCodec` 与 `Provider` 的 trait 方法签名一个都不变，`native_protocol()` 仍是单值（§2.2）。

## 2. 必须在发布期确定的三件事

### 2.1 模型在 path（Gemini provider）

`/v1beta/models/{model}:{operation}` 把两个 body 派生的值放进请求行：物理模型，以及 `generateContent` / `streamGenerateContent` 的选择。而 head 先于 body 写出，所以两者都必须是发布期事实。

PRD R4 的两个选项按实证收敛到选项 1：

- **选项 2（pre-body exchange）不可行**：Pingora 在 `request_filter` 读完整 body 之后不会为重放提供字节（阶段 0 报告 §2 已实测）；convert 路由现有的“扣住全部 chunk、EOS 时 `prepare`”也仍发生在 `upstream_request_filter` 之后。要让 path 依赖 body，只能放弃 Pingora 的代理路径自己发上游请求——那是数据面重写，不是 1.4b。
- **选项 1 采纳并收紧**：`model_map` 在该路由上必须是常量（非空且所有值相同），该唯一物理模型由适配器写进端点 path，同时记录在端点上：

```rust
impl Endpoint {
    /// 端点的 path 已经写进去的模型；`None` 表示模型走 body。
    pub fn model(&self) -> Option<&ModelId>;          // with_model 在构造期设定
}
```

- 适配器在发布期解析不出唯一模型 → `EndpointError::RouteFact { field: "providers[].model_map", reason }`，路由不发布（LKG 继续服务）。
- `Exchange::prepare` 在“解码 → 解析 → 允许列表”之后增加一步：`endpoint.model()` 存在且不等于解析后的物理名 → `ExchangeError::ModelNotAllowed`（403 `model_not_allowed`）。理由：拿用户写的模型去换端点钉住的那个模型是**静默换模型**，是路由不能服务，不是网关缺陷。
- **passthrough 在此类 provider 上被拒绝**：passthrough 的 path 也是发布期值，而该协议的请求目标本身携带模型与操作；固定目标会把调用方字节送去别的模型/操作。检查放在适配器里（`route.mode()` 是路由事实）：`EndpointError::RouteFact { field: "routes[].mode" }`。回到支持的路径是数据面把调用方的请求目标转发给 passthrough 路由（`LlmRoute` 契约变更，独立事项），并且要同时回答“passthrough 绕过 model_map/allowlist”。
- **操作同样钉住**：适配器把 `{operation}` 固定为 `streamGenerateContent?alt=sse`。因此该 provider 的 `decode_response` 面向 **SSE body**：把整段事件流折叠成 `ChatResponse`（§5.3）。客户端看到的答复形状仍由调用方 `stream` 标志决定（数据面契约不变），代价是缓冲答复要折叠而不是解析一个对象文档；替代方案（按 stream 标志选操作）会让一个路由只能服务一种客户端形状，被否决。

### 2.2 一个方言一个 provider kind

OpenAI 原生有两个 wire 方言（Chat Completions 与 Responses），而 `native_protocol()` 是单值、passthrough 合法性是**相等**、`endpoint()` 是发布期单值。保持这些不变量最省的做法是新注册一个 kind：

| kind | `native_protocol()` | 端点 path |
|---|---|---|
| `openai` | `openai.chat` | `…/chat/completions` |
| `openai_responses` | `openai.responses` | `…/responses` |
| `anthropic` | `anthropic.messages` | `…/v1/messages` |
| `gemini` | `gemini.generate_content` | `…/v1beta/models/{model}:streamGenerateContent?alt=sse` |

理由：`kind` 在文档里本来就是不透明 token（`nico-core` 不认厂商），注册表是唯一的“厂商表”，plan 编译器不需要新分支，passthrough 校验不需要从相等变成集合包含，端点/编码/解码都保持单形状。

代价与替代方案（都记录在案，不采纳）：

- 一个 kind 带两个方言（新增 `Provider::dialects()` + 选择规则）会把“passthrough 合法 = 相等”改成“= 属于集合”，并让 `endpoint()` 的选择依赖客户端协议——而客户端协议不该决定上游方言。
- 路由级 `dialect` 配置需要新字段、新校验（值必须在该适配器的方言集合里），且把“一个提供商”拆成两处配置。
- 按客户端协议隐式选方言看起来免费，实际挡死一个真实组合：Responses 客户端打到只讲 chat 的兼容端点（迁移期最常见的情形）。

代价由操作者承担且显式：两个方言想要并存就写两个 provider 条目（它们的 base_url/credential/`model_map` 本来就可能不同）。

### 2.3 输出上限缺省

`/v1/messages` 要求 `max_tokens`，而 canonical `Sampling.max_tokens` 是可选的。用常量补是给调用方的答复悄悄设上限，所以上限是**端点策略**：

- `providers[].max_output_tokens: u32`（可选，>0）。语义：请求没有给上限时，本端点使用的输出上限。文档模型拒绝 0，`ProviderRoute` 用 `NonZeroU32` 承载，所以“设了但不是上限”不可表达。
- 适配器在发布期缺这个值且自己的方言又要求 → `EndpointError::RouteFact { field: "providers[].max_output_tokens", reason }`，路由不发布。Anthropic 适配器是这种适配器；其余适配器忽略该字段（OpenAI/Gemini 都允许省略）。
- `Sampling.max_tokens` 一旦给出，永远优先；适配器不改写 canonical 请求。

## 3. 模块树

```text
crates/nico-llm/src/
├── wire.rs                      # 共享 wire 助手根（crate-private）：只做字节与 JSON，无厂商语义
├── wire/
│   ├── base64.rs                # RFC 4648 编码 + 解码（内联图片/文件字节）
│   └── json.rs                  # parse（只报类别与位置）、error_class、reject_extra（未知成员）
├── protocol.rs                  # 契约不变；新增 3 个 codec 的模块声明
├── protocol/
│   ├── openai_error.rs          # 两个 OpenAI 客户端协议共享的错误包络（新增，第二个真实消费者）
│   ├── anthropic.rs             # anthropic.messages codec
│   ├── anthropic/{request,blocks,response,stream,error,usage}.rs
│   ├── openai_responses.rs      # openai.responses codec
│   ├── openai_responses/{request,items,response,stream,error,usage}.rs
│   ├── gemini.rs                # gemini.generate_content codec
│   └── gemini/{target,request,parts,response,stream,error,usage}.rs
├── provider.rs                  # 契约不变；新增 3 个 adapter 的模块声明与 re-export
├── provider/
│   ├── base_url.rs              # 共享：base_url → Destination + 前缀（无厂商语义，把 1.4a 的解析从 openai/url.rs 抽出）
│   ├── anthropic.rs             + anthropic/{url,request,response,stream,error,usage}.rs
│   ├── openai_responses.rs      + openai_responses/{request,response,stream}.rs
│   ├── gemini.rs                + gemini/{url,request,parts,response,stream,fold,error,usage}.rs
│   └── openai/                  # 既有；`error`/`url` 改为 `pub(super)`（`base64` 迁到 `wire/base64.rs`），供 responses 方言复用
└── (schema/、filter.rs、route/、stream/ 不改)
```

拆分原则沿用 1.4a：一个文件一个数据边界，触到 500 行就按边界继续拆，不按“工具函数”拆。`protocol/<vendor>.rs` 与 `provider/<vendor>.rs` 是厂商名唯一允许出现的地方；`wire/` 加入 `tests/structure.rs` 的 vendor-free 扫描（`protocol/openai_error.rs` 是厂商模块，不在扫描列表里）。

`wire/json.rs` 与 `wire/base64.rs` 的抽取理由是去重（5 个适配器写同一段 `parse`/`error_class`/`reject_extra`；OpenAI 现有的 `provider/openai/base64.rs` 只编码，不解码）且它们是纯函数，没有语义。`provider/base_url.rs` 同理：base_url → destination + 前缀的解析规则（19 种拒绝）与厂商无关，三个厂商的 `url.rs` 各自只负责“前缀 + 自己的操作 path”。

## 4. 客户端 codec

### 4.1 三个新 codec 的共同契约

- **三分表**（1.4a）：modelled → canonical；preserved → 顶层无副作用字段进 `extensions`；refused → `CodecError::UnsupportedField`（422）。modelled 对象体内的未知 key 是坏文档（400 `InvalidRequest`），只报字段路径，不回显值。
- **错误包络**：`encode_error(&CanonicalError)` 用**类别**查一张闭表（5 个 `ErrorCategory` → 该协议的 error type），canonical 的稳定 `code` 只在目标协议真的有字段承载时写出；没有就省略，不凭空造字段。状态取 `CanonicalError::http_status()`，codec 不改写。
- **流式编码器**每响应一个：`push(&StreamEvent)` 返回该协议自己的事件帧，`finish()` 写终止符。凡是 wire 上有 per-response 记账（Responses 的 `sequence_number`、Anthropic 的 content block index、Gemini 的 chunk 组装）都落在这里，codec 单例仍然无状态。
- **`ClientCodec::version()`**：Anthropic → `"2023-06-01"`，Gemini → `"v1beta"`，OpenAI 两个方言 → `None`。

### 4.2 `anthropic.messages`

目标：`POST /v1/messages`（`anthropic-version` 头、`x-api-key`）。codec 不解析 path（该协议的模型在 body 里），上游目标归适配器。

| Canonical | Messages |
|---|---|
| `ChatRequest.model` | `model` |
| 打头的 system/developer `Item::Message` | 顶层 `system`（多个合并；出现在首个非 system item 之后 → `UnsupportedField("conversation[].role")`） |
| user/assistant `Item::Message` | `messages[].role` + `content` blocks |
| `ContentPart::Text` / `Image` | `text` / `image`（`source.type` 为 `base64` 或 `url`） |
| `Item::ToolCall` | assistant message 里的 `tool_use` block |
| `Item::ToolResult` | 下一个 user message 开头的 `tool_result` block（连续结果合并成一条 user message） |
| `Item::Reasoning` | `thinking` block（`signature` → `signature`，text 为空时拒绝） |
| `Tool` / `ToolChoice` | `tools[]`（`input_schema`）/ `tool_choice`（`auto`/`any`/`tool`/`none`）；`disable_parallel_tool_use` 无 canonical 位置 → 422 `unsupported_field` |
| `Sampling` | `temperature`、`top_p`、`max_tokens`（必填）、`stop_sequences`；`seed`/`presence_penalty`/`frequency_penalty` 无字段 → 拒绝 |
| `ResponseFormat` | 无（`json_object`/`json_schema` → `Unsupported{ResponseFormat}`） |
| `FinishReason` | `end_turn`/`stop_sequence` → `Stop`；`max_tokens` → `Length`；`tool_use` → `ToolCalls`；`refusal` → `ContentFilter`；`pause_turn`/未知 → `Malformed`（server tool 不在范围内，把它说成 `Stop` 是谎报） |
| `Usage` | `input_tokens`/`output_tokens`/`cache_read_input_tokens`/`cache_creation_input_tokens`，按报告值原样记录 |

- 客户端请求缺 `max_tokens` → `InvalidRequest`（对方协议自己要求它，不是 canonical 缺口）。
- `temperature > 1.0` → `InvalidRequest`（该协议自己的范围；canonical 取最宽者，codec 在自己协议范围外拒绝，不 clamp）。
- SSE：`message_start` / `content_block_start` / `content_block_delta`（`text_delta`、`input_json_delta`、`thinking_delta`、`signature_delta`）/ `content_block_stop` / `message_delta`（`stop_reason`、`usage`）/ `message_stop` / `ping` / `error`。`ping` 与未知事件类型跳过（心跳不是内容）；`error` 事件 → `StreamEvent::Error`。
- 客户端 encoder 的 index 记账：文本块、thinking 块、每个 `tool_use` 块各占一个 `content_block` index，由 `ToolCallStart.index` 与事件顺序分配；`Finish` 与 `Usage` 都写成 `message_delta`（可多次），`message_stop` 由 `finish()` 写出——所以 provider 在 finish 之后才报 usage 时仍排在终止符之前。
- **thinking 开关是一个已知缺口**：`thinking: {type:"enabled", budget_tokens}` 没有 canonical 位置 → 422 `unsupported_field("thinking")`。请求里回放的 `thinking` block 仍映射成 `Item::Reasoning`（§6 里因此只有 provider 侧不声明 `reasoning`）。

### 4.3 `openai.responses`

目标：`POST /v1/responses`。请求 `input` 是 item 数组（或一个字符串），响应是 `output` item 数组，流是一串 `response.*` 事件。

| Canonical | Responses |
|---|---|
| `ChatRequest.model` | `model` |
| 打头 system/developer `Item::Message` | `instructions`（多个合并；出现在中间 → `UnsupportedField`） |
| user/assistant `Item::Message` | `input` 里的 `{type:"message", role, content:[…]}`；文本 part 为 `input_text`（user/system）/ `output_text`（assistant） |
| `ContentPart::Image` | `input_image`（`image_url`：http(s) 或 base64 data URL） |
| `Item::ToolCall` | `{type:"function_call", call_id, name, arguments}` |
| `Item::ToolResult` | `{type:"function_call_output", call_id, output}` |
| `Item::Reasoning` | 无字段承载（该协议需要 `id` + `encrypted_content` 两个句柄）→ `Unsupported{Reasoning}`（§6） |
| `Tool` | `{type:"function", name, description, parameters}`（注意不是 chat 的嵌套 `function`） |
| `ToolChoice` | `"auto"` / `"none"` / `"required"` / `{type:"function", name}` |
| `Sampling` | `temperature`、`top_p`、`max_output_tokens`；`stop`/`seed`/两个 penalty 无字段 → 拒绝 |
| `ResponseFormat` | `text.format`：`text` 省略、`json_object`、`json_schema` |
| `FinishReason` | `status: completed` + 有 function_call → `ToolCalls`；`incomplete_details.reason: max_output_tokens` → `Length`；`content_filter` → `ContentFilter`；`failed` → `Error` |
| `Usage` | `input_tokens`/`output_tokens`/`input_tokens_details.cached_tokens`/`output_tokens_details.reasoning_tokens` |

- **拒绝**（`UnsupportedField`，422）：`previous_response_id`（网关无会话存储）、`store: true`（网关无状态且不替调用方开启留存；adapter 恒写 `store: false`）、`reasoning.effort`/`summary`、内置工具（`web_search`/`file_search`/`computer_use`/`code_interpreter`/`mcp`）、`truncation`、`parallel_tool_calls`、`include`、`text.verbosity`、`service_tier`、`background`、`prompt` 模板引用。`metadata`/`user` 是无副作用标签 → `extensions`。
- `function_call` item 的 `id`（`fc_…`）是 provider 的 item 记账，不是会话内容（配对键是 `call_id`）→ 不保留、不回显；`reasoning` item 的 `id` 不同，它指向内容本身，所以 reasoning 整体按“无法承载”处理。
- SSE：`response.created` / `response.in_progress` / `response.output_item.added` / `response.content_part.added` / `response.output_text.delta` / `.done` / `response.function_call_arguments.delta` / `.done` / `response.reasoning_summary_text.delta` / `response.output_item.done` / `response.completed` / `response.failed` / `error`；其他类型跳过。
- 客户端 encoder：`Start` → `response.created`（`in_progress` 随后），文本/工具增量为各自事件，`sequence_number` 递增；`finish()` 之前必须写出 `response.completed`，它携带累积出的完整 `output` 数组、`status`、`usage`，因此该 encoder 累积文本与工具参数（超过 `max_response_bytes` 的答复本来就被缓冲上限约束，流式累积只保留答复本身）。Responses 流没有 `[DONE]`，`finish()` 只关闭未完成的 item。

### 4.4 `gemini.generate_content`

目标：`POST /v1beta/models/{model}:generateContent` 或 `:streamGenerateContent?alt=sse`——**模型与操作都在请求目标里**。codec 从 `RequestFacts::path()`（含 query）解析：

- `…/models/{model}:{operation}`，`{operation}` ∈ {`generateContent`, `streamGenerateContent`}；其他（`countTokens`/`embedContent`/`predict`）→ `InvalidRequest`（不在本 codec 的文档范围）。`stream` 由 `streamGenerateContent` 决定。
- `alt`：缺省或 `sse` 接受（流式答复恒为 SSE）；其他值 → `UnsupportedField("alt")`。
- 模型名来自 path；body 里的 `model` 字段不存在（Gemini 没有它）。

| Canonical | generateContent |
|---|---|
| `Item::Message` user/assistant | `contents[]`，role `user`/`model` |
| system/developer `Item::Message` | `systemInstruction.parts`（首段之外 → `UnsupportedField`） |
| `ContentPart::Text` / `Image` | `parts[].text` / `parts[].inlineData{mimeType,data}`；**URL 图片无字段**（`fileData.fileUri` 是 File API 的引用）→ `UnsupportedField("content_part.image.url")` |
| `Item::ToolCall` | `parts[].functionCall{id?,name,args}` |
| `Item::ToolResult` | 同一 user turn 的 `parts[].functionResponse{id?,name,response}`；canonical 只有文本 part，`response` 必须是对象 → 写 `{"content": <joined text>}`，图片 part → `UnsupportedField("tool_result.content")`；`is_error` 无字段 → 拒绝 |
| `Item::Reasoning` | `parts[].thought = true` + `thoughtSignature`（= canonical signature） |
| `Tool` / `ToolChoice` | `tools[].functionDeclarations[]` / `toolConfig.functionCallingConfig.mode`（AUTO/ANY/NONE）+ `allowedFunctionNames` |
| `Sampling` | `generationConfig.{temperature,topP,maxOutputTokens,stopSequences,presencePenalty,frequencyPenalty,seed}`；`seed` 与两个 penalty 只在该协议支持时写出，不支持则拒绝 |
| `ResponseFormat` | `generationConfig.responseMimeType = application/json` + `responseSchema` |
| `FinishReason` | `STOP` → `Stop`；`MAX_TOKENS` → `Length`；`SAFETY`/`RECITATION`/`BLOCKLIST`/`PROHIBITED_CONTENT`/`SPII` → `ContentFilter`；`MALFORMED_FUNCTION_CALL`/`OTHER` → `Error`；未知 → `Malformed` |
| `Usage` | `usageMetadata.{promptTokenCount,candidatesTokenCount,cachedContentTokenCount,thoughtsTokenCount}` |

- 拒绝：`candidateCount > 1`、`safetySettings`、`cachedContent`、`tools[].googleSearch`/`codeExecution`/`urlContext`、`thinkingConfig`（canonical 无预算字段）、`generationConfig.topK`（`UnsupportedField("top_k")`，§10）。`labels` → `extensions`。
- **call id 的缺口**：`functionCall.id` 可选，canonical `ToolCallId` 必填 → decoder 在缺失时按位置分配 `call_<index>`（同一响应内稳定），并在下一次编码时作为 `id` 写出，不依赖 provider 回显。
- **响应 id 的缺口**：`GenerateContentResponse.responseId` 有则用作 `ResponseId`；没有则用常量 `"gemini-response"`（该字段不是会话内容，客户端只把它当标识）。记录在 §10。
- 流式：每个 SSE `data:` 是一个 `GenerateContentResponse` 分片；`parts[]` 里的文本 → `TextDelta`，`thought` 文本 → `ReasoningDelta`，`functionCall` → `ToolCallStart` + `ToolCallEnd`（该协议一次给全参数，没有 fragment），`usageMetadata` → `Usage`，`finishReason` → `Finish`。客户端 encoder 反向：文本增量一个 chunk 一个 part；工具调用的 JSON 参数须累积到 `ToolCallEnd` 才能写成完整 `args` 对象（该协议没有参数增量），`finish()` 时仍未闭合 → `CodecError::Encode`（canonical 流总会闭合，未闭合是网关缺陷）。

## 5. provider adapter

### 5.1 `anthropic`

- `endpoint`：`/v1/messages`（base_url 是 API 前缀，规则与 `provider/openai/url.rs` 同一套、且用同一个 `provider/base_url.rs`：origin 或 `/` → `/v1/messages`，带前缀 → `<prefix>/v1/messages`），headers `content-type`、`accept: application/json, text/event-stream`、`x-api-key: <expose()>`、`anthropic-version: 2023-06-01`。**发布期要求 `max_output_tokens`**（§2.3）。
- `encode`：§4.2 的表反向。`max_tokens` = `Sampling.max_tokens` 或路由缺省。`thinking` 请求不写出（无法启用），因此 `Item::Reasoning` → `UnsupportedField("conversation[].reasoning")`，`capabilities()` 不声明 `reasoning`。
- `decode_response`：`content` blocks 折叠进一个 `AssistantTurn`（text → content part，`tool_use` → `ToolCall`，`thinking` → `Reasoning`），`stop_reason` → `FinishReason`，`usage` 原样。
- `stream_decoder`：`message_start`（id/model/usage）→ `Start` + 首个 `Usage`；`content_block_start`/`delta`/`stop` → 文本/工具/thinking 事件；`message_delta` → `Usage`/`Finish`；`message_stop` 结束（其后字节忽略）；`error` → `Error`；未闭合的 block 或未宣布的 index → `Malformed`；`finish()` 前没有 `stop_reason` → `Malformed`（沿用 1.4a 的完整性标记规则：不能把没人宣布过的结束告诉调用方）。
- 失败状态表（`error_for_status`）用 Anthropic 自己的 `error.type` 只进日志；状态映射沿用 1.4a 的类别/状态规则（400 保留、401/403 → 502、404 保留、429 保留、5xx → 502、其他 → 502）。

### 5.2 `openai_responses`

- 新 kind `openai_responses`，`native_protocol()` = `openai.responses`，复用同一厂商目录里的 `provider/openai/{error,url}`（改为 `pub(super)`；`url::endpoint` 增加一个 path 后缀参数，chat 传 `/chat/completions`，responses 传 `/responses`）；chat 自己的 `usage` 形状与 responses 不同，不共享。
- `encode`：§4.3 的表反向；恒写 `store: false`。
- `decode_response`：`output[]` 折叠进 `AssistantTurn`（`message` 的 `output_text` → content part，`function_call` → `ToolCall`；`reasoning` 的 `summary` 文本渲染为 `AssistantTurn::reasoning`（signature 为空），`id`/`encrypted_content` 不进 canonical，因此不可回放——见 §6），`status`/`incomplete_details` → `FinishReason`，`usage` 原样。`output[]` 里出现未知 item 类型 → `Malformed`（不是静默跳过：未知 item 可能改变调用方看到的内容）。
- `stream_decoder`：`response.created` → `Start`；`response.output_text.delta` → `TextDelta`；`response.function_call_arguments.delta/.done` → `ToolCallStart/Delta/End`（`item_id`/`output_index` 分配 canonical `index`）；`response.completed` → `Usage` + `Finish`；`response.failed`/`error` → `Error`；其他事件跳过。`finish()` 前没有 `response.completed` → `Malformed`。

### 5.3 `gemini`

- `endpoint`：base_url 前缀 + `/v1beta/models/{physical}:streamGenerateContent?alt=sse`（§2.1），模型来自 `model_map` 的唯一值，`Endpoint::with_model(physical)`；headers `content-type`、`accept: text/event-stream`、`x-goog-api-key: <expose()>`；`version()` → `v1beta`。拒绝：`model_map` 非常量（`RouteFact`）、`mode: passthrough`（`RouteFact`）。
- `encode`：§4.4 的表反向；`stream` 不写（该协议没有这个字段）。
- `decode_response`：把 body 当 SSE 解析 → 折叠（§5.3.1）→ `ChatResponse`。
- `stream_decoder`：与 codec 的反向映射同表；`fold` 复用同一个事件源。

**5.3.1 fold（事件 → `ChatResponse`）**：`provider/gemini/fold.rs` 是 crate 内部的一个小状态机，输入是 `StreamEvent`，输出是 `ChatResponse`（`Start` 提供 id/model，文本/thinking 拼接，`ToolCallStart/Delta/End` 拼 JSON，`Usage` 取最后一个，`Finish` 取最后一个）。它只被 Gemini 的 `decode_response` 使用；出现第二个消费者（例如某个 provider 只提供事件流）时再提升到 canonical 侧，现在不抽象。

## 6. 能力矩阵

`capabilities()` 只声明**真的映射过**的东西（1.4a 的信任规则）。请求期 `filter::required()` 计算需求并与交集比较，缺一个就是 422 `unsupported_capability`。

| | streaming | tools | images | files | reasoning | response_format |
|---|---|---|---|---|---|---|
| codec `openai.chat`（既有，本次补上 data URL） | ✓ | ✓ | ✓ url+data | — | ✓ | ✓ |
| codec `openai.responses` | ✓ | ✓ | ✓ url+data | — | — | ✓ |
| codec `anthropic.messages` | ✓ | ✓ | ✓ url+base64 | — | ✓ | — |
| codec `gemini.generate_content` | ✓ | ✓ | ✓ inline | — | ✓ | ✓ |
| adapter `openai`（既有） | ✓ | ✓ | ✓ url+data | — | ✓ | ✓ |
| adapter `openai_responses` | ✓ | ✓ | ✓ url+data | — | — | ✓ |
| adapter `anthropic` | ✓ | ✓ | ✓ url+base64 | — | — | — |
| adapter `gemini` | ✓ | ✓ | ✓ inline | — | ✓ | ✓ |

“(client, provider)” 的每个组合都有明确结论：交集为空不是发布失败（纯文本不需要能力），但表里任何一格为 `—` 就意味着该组合对这个能力返回 422。三处 `reasoning` 的 `—` 与两处 `response_format` 的 `—` 是有理由的：

- `openai.responses` 两侧都不声明 `reasoning`：该协议回放 reasoning 需要 `(id, encrypted_content)` 两个句柄，canonical `Reasoning` 只有一个不透明 `signature`。**响应侧的 reasoning summary 仍然会渲染**（`AssistantTurn::reasoning`，signature 为空）；不能回放，因此重放请求在能力门被拒。协议演进（canonical 增加第二个句柄）是重新打开这一格的唯一条件。
- `anthropic` 适配器不声明 `reasoning`：写回 `thinking` block 要求请求同时启用 thinking 预算，而 canonical 没有这个旋钮；写一个 block 而不启用它会被对方拒。客户端 codec 声明它（它映射了 `thinking` block 的两种方向）。
- `anthropic` 的 `response_format` 两侧都不声明：该协议没有 JSON 模式。

三类拒绝的归属固定（沿用 1.4a）：

| 情形 | 错误 | 状态 |
|---|---|---|
| 请求需要某一能力，交集里没有 | `ExchangeError::Unsupported` | 422 `unsupported_capability` |
| 已知字段/取值在 canonical 里没有位置（`top_k`、thinking 预算、`stop` 在 Responses） | `CodecError`/`ProviderError::UnsupportedField` | 422 `unsupported_capability` |
| 文档不是该协议的样子（缺必填、role 不认识、未知嵌套成员、范围越界） | `InvalidRequest` | 400 `invalid_request` |
| 上游答复读不懂（未知 finish reason、未闭合块、折叠失败） | `Malformed` | 502 `upstream_protocol_error` |

## 7. 流式

- 复用 `stream/sse.rs` 的 framing（任意分片、跨块 UTF-8、多行 `data:`、心跳、终止符），不新增 SSE 解析器。OpenAI 的 `[DONE]` 只属于该协议；Anthropic 以 `message_stop`、Responses 以 `response.completed` 结束、终止符都由各自的 `ClientStreamEncoder::finish()` 写出。
- 事件顺序与分片无关：`StreamEncoder` 逐事件驱动两侧。provider 在 `Finish` 之后才报 usage 的形态在四个客户端协议下都必须落在终止符之前（Anthropic 写成第二个 `message_delta`；OpenAI chat 是独立的 usage chunk；Responses 写进 `response.completed`；Gemini 写进最后一帧的 `usageMetadata`）。
- 背压仍由数据面写路径与请求预算承担，transform 不建队列；`max_response_bytes` 只约束缓冲答复（流式豁免，与 1.4a 一致）。

## 8. 测试与 fixtures

- **矩阵表**（`tests/matrix.rs`）：4 个 codec × 4 个 adapter 的全部 16 行，每行断言三件事——两侧都在注册表里、能配对成 `Exchange`、`exchange.capabilities()` 等于表里的期望集合。表是显式的（不从注册表推导），少一个 codec 或一个 adapter 就是失败。
- **passthrough 表**：`(protocol, kind)` 的 4 行原生对齐，3 行可发布，`gemini` × `gemini` 断言发布期拒绝（并且断言拒绝原因是路由事实而不是 endpoint 解析）。
- **共享 fixture**（`tests/fixtures/matrix/`）：一份 canonical 对话（含文本、工具调用/结果、**内联图片**、reasoning、stop reason、usage）加每个协议各一份 wire 文档。内联图片是唯一能在四个协议里都写出的拼写（Gemini 没有 URL 图片字段），所以 1.4b 顺带让 `openai.chat` 客户端 codec 接受 `data:` URL 图片（base64 解码就绪后不再需要拒绝），否则“同一对话四个协议”的 fixture 无法成立。测试三件事：`codec.decode(wire) == canonical`、`codec.encode(canonical) == wire`（语义比较，不比字节）、`adapter.encode(canonical) == wire_provider`、`adapter.decode(wire_provider) == canonical`。“语义相等”的定义写在一个助手里：逐项比较 `model`/`stream`/`tools`/`tool_choice`/`response_format` 与规范化后的 `conversation`（item 顺序、role、part 拼写）；**`extensions` 不参与比较**——它装的正是各方私有字段。四套 fixture 就是 R5 的“同一对话在四个协议下语义等价”，跨协议翻译由矩阵 e2e 断言（`anthropic.messages` 客户端 → `openai` 上游，再反向）。
- **每协议测试目录**（沿用 `tests/openai_chat/` 与 `tests/openai_provider/` 的组织）：`tests/anthropic_*`、`tests/openai_responses_*`、`tests/gemini_*`；每个目录覆盖请求 decode、响应 encode、流式 encode、错误包络、任意 1..N 字节分片、以及该协议独有的拒绝表。
- **端到端**（`nico-server/tests/`）：新增 `llm_anthropic.rs`（Anthropic 客户端 → OpenAI 上游，convert）与 `llm_gemini.rs`（OpenAI 客户端 → Gemini 上游：断言上游 path 含钉住的模型、`alt=sse`、非流式答复从 SSE 折叠、模型不匹配返回 403、passthrough 发布失败）。既有的 `llm_convert.rs` 不动。
- **结构**：`tests/structure.rs` 的扫描列表加入 `wire`；`protocol/<vendor>.rs`/`provider/<vendor>.rs` 仍豁免（`protocol/openai_error.rs` 属于后者）“vendor 名只出现在厂商模块”的规则不变。文件预算 ≤500 行，`tests/*.rs` 同样适用。
- **门**：五条基线（fmt / clippy / test / deny / doc）+ `cargo check --no-default-features`；`cargo tree -p nico-http | grep nico-llm` 仍无输出。

## 9. 观测与 spec

- 装配期：每 codec/adapter 一行 `info`（`llm.protocol`/`llm.provider` + 能力 + 有则 `llm.version`）——从 2 行变成 8 行，字段不变。
- 请求期：`llm.protocol` / `llm.provider` / `llm.model`（逻辑 → 物理）/ `llm.stream` / `config.generation`，不新增字段。`llm.provider` 的稳定拼写是四个 kind：`openai`、`openai_responses`、`anthropic`、`gemini`。
- 不记录 prompt/答复 body、`x-api-key`、`Authorization`、`x-goog-api-key`；`Endpoint`/`RequestFacts`/`ResponseFacts` 的 `Debug` 仍只打印 header 名。
- spec 更新：`directory-structure.md`（新模块树 + `wire/`）、`logging-guidelines.md`（四个 kind 拼写、`llm.version` 取值）、`error-handling.md`（三张新状态/包络表与 422 的字段拒绝）、`pingora-guidelines.md`（模型在 path ⇒ 发布期钉住 + 恒流式 + fold，passthrough 拒绝的理由）。

## 10. 风险与已知缺口

| 风险 / 缺口 | 处置 |
|---|---|
| Gemini 端点恒用流式操作，缓冲答复从 SSE 折叠 | 客户端可见形状不变；代价是折叠这一步（`gemini/fold.rs`，只被 `decode_response` 使用）。若某天数据面允许请求期 path，折叠可以被“按 stream 标志选操作”取代 |
| Gemini passthrough 被拒 | 请求目标本身携带模型与操作；转发固定目标会送错模型。回到支持需要数据面转发调用方 target，并回答 allowlist 绕过问题 |
| `top_k`（Anthropic/Gemini 都有） | canonical `Sampling` 冻结，不新增旋钮 → 422 `unsupported_field("top_k")`。要支持必须先改 `schema/` 的 knob 集合，属于 canonical 修订 |
| 1.4a 的 OpenAI 客户端 codec 曾拒绝 `data:` 图片 URL | 本次补上（`wire/base64.rs` 提供解码），否则矩阵 fixture 缺少一种四协议通用的图片拼写 |
| thinking 预算（Anthropic）/ `thinkingConfig`（Gemini） | 同上，422；Anthropic 适配器因此不声明 `reasoning` |
| 流式 thinking 的 `signature_delta`（Anthropic） | `StreamEvent` 只有 `ReasoningDelta(text)`，没有放签名的位置（1.4a 冻结的形状）。流里丢弃并在模块文档写明，完整响应里仍保留 `signature`。要修必须先改 canonical 事件集 |
| Responses 的 reasoning 句柄是二元组 | 两侧都不声明 `reasoning`；响应侧仍渲染 summary。重新打开这一格需要 canonical 增加第二个不透明句柄 |
| `ContentPart::File` | 四个协议都不声明 `files`（1.4a 的结论沿用）：canonical 携带内联字节，而三家都用“已存储文件”的引用形态。请求带 file part → 422 |
| Gemini 响应没有 `responseId` 时用常量 | 该字段不是会话内容；记录在此，客户端看到的是稳定但非唯一的标识 |
| usage 不做跨厂商归一化 | Anthropic 的 `input_tokens` 不含缓存、OpenAI 的 `prompt_tokens` 含、Gemini 的 `promptTokenCount` 含 `cachedContentTokenCount`。原样记录（1.4a 已声明“不做归一化”），计费留阶段 2 |
| Responses wire 面比另两家新，字段集合需要实证 | 实施第 4 步开头有一个研究/实证检查点：把适配器写/读的每个字段对一遍厂商参考文档，结果记进 `research/responses-wire.md`；实证不到的一律落成 typed 拒绝，不做静默省略 |
| 客户端 `alt` 非 `sse` | 422 `unsupported_field("alt")`：本网关的流式答复恒为 SSE |
| 明文 HTTP / 无 TLS | 仍用 mock 上游验收；真实 https 端点需要独立 TLS 任务（沿用 1.4a） |
| 模型 fallback / 熔断 / 重试预算 | 阶段 2；本任务只保证 canonical 可重放（1.4a 已成立） |

## 11. 交付边界

- **`nico-llm`**：`wire/`（base64 + json 助手）；`provider/base_url.rs`（从 `provider/openai/url.rs` 抽出的共享解析）；`protocol/{anthropic,openai_responses,gemini}.rs` 与其子模块；`protocol/openai_error.rs`；`protocol/openai_chat/message.rs`（接受 `data:` 图片 URL）；`provider/{anthropic,openai_responses,gemini}.rs` 与其子模块；`provider/openai/{error,url}` 的 `pub(super)` 与 `url::endpoint` 的后缀参数；`provider/endpoint.rs`（`model` + `RouteFact`）；`provider/route.rs`（`max_output_tokens`）；`filter.rs`（`prepare` 的钉模型检查）；`lib.rs` 的 `builtin_registry()` 与模块文档。
- **`nico-core`**：`providers[].max_output_tokens`（形状 + 语义校验 + 文档模型 + 默认缺省）；不改 canonical，不改 route 形状。
- **`nico-server`**：`llm/plan.rs` 把 `max_output_tokens` 编进 `ProviderRoute`；新增两个端到端测试目标。
- **spec**：§9 的四份。
- **不在本任务**：`schema/` 修订（`top_k`、thinking 预算、reasoning 第二句柄）、passthrough 的请求目标转发、TLS、fallback/熔断/计费。
