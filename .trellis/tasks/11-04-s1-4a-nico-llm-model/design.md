# nico-llm 技术设计：canonical 模型与两条协议缝

> 需求见 `prd.md`。1.4a 只做 OpenAI Chat 一条链路，但所有形状与缝都必须能容纳 1.4b 的 Anthropic / Responses / Gemini，否则就是又一次“先写逻辑、后补模型”。

## 1. 不变量

1. **两个独立轴**：客户端协议（OpenAI Chat / Responses / Anthropic Messages / Gemini）与上游 provider（OpenAI / Anthropic / Gemini / …）互不推导。两者之间只有一个 canonical 模型。
2. **形状先行**：`schema/` 不依赖 `protocol/`、`provider/`、Pingora 或 `nico-http`；协议转换逻辑只能消费 canonical 形状。
3. **核心不 switch 厂商**：`schema/` 与 `filter.rs` 不出现厂商名/分支；厂商名只出现在 `protocol/<name>` 与 `provider/<name>`。新增厂商只加一个文件，不改核心。
4. **请求可重放**：convert 模式的 AI 路由有界缓冲客户端 body，canonical 是值；重试/fallback 重新编码，不重放原始 body，也不要求 Pingora 重放。
5. **响应永不整块**：SSE 逐块 `provider chunk → canonical event → client chunk`；非流式响应才允许有界缓冲。
6. **失败闭集**：未知 protocol/provider kind、缺失能力、插件与 AI 缓冲冲突都在发布期拒绝；运行期不支持的能力返回 typed error，不静默丢字段。
7. **数据面仍是唯一写出点**：连接、TLS、预算、帧化、错误写出归 `nico-http`；`nico-llm` 只产出 `Endpoint`（destination/path/headers）、wire body 与响应转换器。
8. **raw 逃生口**：客户端协议与 provider 原生协议一致且路由声明 `mode: passthrough` 时跳过 canonical，字节直通；convert 是默认。

## 2. 模块树

```text
crates/nico-llm/src/
├── lib.rs              # //! + pub use schema/contracts; builtin registries
├── schema.rs           # canonical 类型根；只做 re-export 与文档
├── schema/
│   ├── request.rs      # ChatRequest / Sampling / ResponseFormat
│   ├── conversation.rs # Item / Message / ContentPart / Role
│   ├── tool.rs         # Tool / ToolCall / ToolResult / ToolChoice
│   ├── extension.rs    # RawJson / Extensions：唯一的无类型值及其边界
│   ├── response.rs     # ChatResponse / Choice / AssistantTurn / FinishReason
│   ├── stream.rs       # StreamEvent
│   ├── usage.rs        # Usage（measured / reported）
│   ├── error.rs        # CanonicalError
│   └── model.rs        # ModelId / ModelRef（逻辑名与物理名）
├── protocol.rs         # ClientCodec trait + ClientReply + ProtocolRegistry + Capabilities
├── protocol/
│   ├── facts.rs        # RequestFacts / ResponseFacts：框架无关的交换事实，Debug 只印 header 名
│   └── openai_chat.rs  # 1.4a：OpenAI Chat 客户端 codec
├── provider.rs         # Provider/ProviderStream trait + ProviderRegistry
├── provider/
│   ├── route.rs        # ProviderRoute / Credential / ExchangeMode：编译后的路由事实
│   └── openai.rs       # 1.4a：OpenAI 上游适配（endpoint/encode/decode/stream）
├── stream/
│   └── sse.rs          # 共享 SSE framing（事件边界/跨块 UTF-8），无厂商语义
├── filter.rs           # 路由交换：endpoint → decode → encode → response/stream
└── filter/
    ├── error.rs        # ExchangeBuildError / ExchangeError 与 canonical 映射
    └── stream.rs       # StreamEncoder：每响应一个 provider decoder
```

每个文件 ≤ 500 行；`schema.rs` 在接近上限时按上面的数据边界继续拆，不按“工具函数”拆。

模型映射不单独建模块：`schema/model.rs` 只有 `ModelId`/`ModelRef` 两个值类型，逻辑→物理的解析是 `ProviderRoute::model`（编译后的 `model_map` 是路由事实，请求期只读）。第二个真实消费者出现之前不抽新文件。

## 3. Canonical 形状

```rust
pub struct ChatRequest {
    model: ModelId,                     // 客户端写的逻辑模型名
    conversation: Vec<Item>,            // 不直接照抄任何一个厂商的 messages[]
    tools: Vec<Tool>,
    tool_choice: ToolChoice,
    sampling: Sampling,
    response_format: ResponseFormat,
    stream: bool,
    extensions: Extensions,             // 未建模字段，原样保留
}

pub enum Item {
    Message(Message),                   // Message { role: Role, content: Vec<ContentPart> }
    ToolCall(ToolCall),                 // ToolCallId + ToolName + RawJson arguments
    ToolResult(ToolResult),             // ToolCallId + content + is_error
    Reasoning(Reasoning),               // text + optional provider signature
}

pub enum ContentPart {
    Text(String),
    Image(ImageSource),                 // url 或 base64；不含厂商字段
    File { media_type: String, data: Vec<u8>, name: Option<String> },
}

pub struct ChatResponse {
    id: ResponseId,
    model: ModelId,                     // 物理模型（provider 返回）
    choices: Vec<Choice>,
    usage: Usage,
    extensions: Extensions,
}

pub enum StreamEvent {
    Start { id: ResponseId, model: ModelId },
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallStart { id: ToolCallId, name: ToolName },
    ToolCallDelta { id: ToolCallId, arguments: String },
    ToolCallEnd { id: ToolCallId },
    Usage(Usage),
    Finish(FinishReason),
    Error(CanonicalError),
}
```

字段私有：构造器与 `with_*` 校验各自的输入，`validate()` 校验整棵树（包括扩展键）；上面的代码只表达形状。

三个形状决定，分别对应一类“会烂掉”的地方：

- **`Item` 而不是 `messages[]`**：Responses 的 `input` items、Anthropic 的 content blocks、Gemini 的 parts 都能映射到同一组 item；tool call/result 与 reasoning 是一等 item，不用字符串编码。`role` 只保留 system/developer/user/assistant 四个语义值；tool 答案是 `Item::ToolResult`，不是 role，因为只有它能携带所属的 call id。
- **`StreamEvent` 而不是 OpenAI chunk**：OpenAI `chunk`、Anthropic `content_block_delta`、Responses `response.output_text.delta`、Gemini candidate parts 都归一到同一组增量事件；客户端 codec 再按自己的 wire 编码。否则 OpenAI chunk 会变成事实标准，新增客户端协议时反向污染 provider adapter。
- **`RawJson` 而不是到处 `serde_json::Value`**：调用方声明的 JSON 文档（tool parameters、tool arguments、structured-output schema）与未建模字段用显式 newtype 包装（定义在 `schema/extension.rs`），只允许出现在 `schema/tool.rs`、`schema/request.rs`（`JsonSchema`）与 `Extensions`；配置、路由和 `PluginContext` 仍禁止 `Value`。未知字段必须进 `extensions`，不能静默丢。

`Usage` 同时保留 provider-reported 与网关可测量的字段（input/output/cache_read/cache_write/reasoning），1.4a 只定义形状，不做计费/归一化。`CanonicalError` 只带安全 message、稳定 code、类别、`http_status` 与 retryable；provider 原始错误体留在 provider adapter 的日志上下文里。

## 4. 契约

### 4.1 客户端 codec

```rust
pub trait ClientCodec: Send + Sync {
    fn protocol(&self) -> ProtocolId;                 // "openai.chat" ...
    fn capabilities(&self) -> Capabilities;
    fn decode_request(&self, facts: &RequestFacts, body: &[u8]) -> Result<ChatRequest, CodecError>;
    fn encode_response(&self, response: &ChatResponse) -> Result<ClientReply, CodecError>;
    fn encode_stream_event(&self, event: &StreamEvent) -> Result<Vec<u8>, CodecError>;
    fn encode_error(&self, error: &CanonicalError) -> Result<Vec<u8>, CodecError>;
}
```

`RequestFacts` 是 owned 的 method/path/headers（不含 Pingora 类型），因为客户端协议可能需要路径或头（例如 Gemini 的模型在 URL 里）；body 已在 `schema` 之外被缓冲。它与 `ResponseFacts` 一起落在 `protocol/facts.rs`：两者都是“交换的事实”，与 codec/provider 的契约分开，`Debug` 只印 header 名。

`ClientReply { status, body }` 也定义在 `protocol.rs`，因为**成功状态属于客户端协议**：一个完整答复在客户端协议里恒为 `200`，而 provider 的 `201`/`206` 描述的是它自己怎么帧化 body，重新编码后对调用方没有意义。失败状态相反：它属于 `CanonicalError::http_status`，所以 `encode_error` 只返回 body。

### 4.2 上游 provider

`ProviderRoute` 是编译后的路由事实：`route_id`、`protocol`、`provider_kind`、`model_map`、`mode`，以及已解析的 base_url/credential 句柄；它由 `LlmPlan` 在发布期构建，请求期只读。

```rust
pub trait Provider: Send + Sync {
    fn kind(&self) -> ProviderKind;                    // "openai" ...
    fn capabilities(&self) -> Capabilities;
    fn endpoint(&self, route: &ProviderRoute) -> Result<Endpoint, ProviderError>;
    fn encode(&self, route: &ProviderRoute, request: &ChatRequest) -> Result<Vec<u8>, ProviderError>;
    fn decode_response(&self, head: &ResponseFacts, body: &[u8]) -> Result<ChatResponse, ProviderError>;
    fn stream_decoder(&self) -> Box<dyn ProviderStream>;
}

pub trait ProviderStream: Send {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<StreamEvent>, ProviderError>;
    fn finish(&mut self) -> Result<Vec<StreamEvent>, ProviderError>;
}
```

- `endpoint()` 只依赖路由/provider 配置（base_url、协议、固定 path），**不依赖请求体**。这样上游请求头可以在 body 解码前确定；`encode()` 在 body 缓冲后生成 wire body。OpenAI/Anthropic/Responses 的 endpoint 是固定路径，满足这一条；Gemini 的 path 含模型名，是 1.4b 的专门设计项（见 §10）。
- `encode(route, request)` 接收路由，因为物理模型来自 `route.model()` 对编译后 `model_map` 的解析；adapter 因此保持单例无 per-request 状态。canonical 请求保留逻辑名，重试或换路由时重新解析。
- `Endpoint { destination, path, headers }` 与 `encode()` 的 body 由 `Exchange` 组装成 `WireRequest { endpoint, body }`。method 不在其中：chat 交换恒为 `POST`，由数据面写出。将来出现非 POST 能力时，动词是 `Provider` 契约的显式扩展，不会从 path/header 里猜。
- provider 不拥有连接、TLS、超时、重试或帧化；这些仍归数据面。

### 4.3 capability matrix

`Capabilities` 是一组命名能力（streaming/tools/images/files/reasoning/response_format），不是厂商分支。发布期求 codec 与 provider 的交集并记录；**空交集本身不拒绝发布**，因为纯文本不需要任何 capability。请求期由 `Exchange` 计算请求需要的集合（stream、tools、response_format、image/file part、reasoning）并与交集比较，缺一个就返回 `ExchangeError::Unsupported`，客户端得到 422 `unsupported_capability`，不静默丢字段。1.4a 的 OpenAI×OpenAI 支持 streaming/tools/response_format；images/files 先标记为 capability，映射在 1.4b/后续逐个补。

## 5. 路由与发布

- `routes[].protocol` 只对 `RouteTarget::Provider` 必填，字符串如 `openai.chat`；`routes[].mode: convert | passthrough` 默认 `convert`。
- `providers[]` 增加可选 `model_map: BTreeMap<String, String>`；凭证仍在发布期 materialize，provider 适配器只拿到已解析的值。
- `LlmPlan` 在 `nico-server` 的 reload 中与快照同一函数编译：按 `protocol` 查 codec、按 `provider.kind` 查 provider、检查 route 的 `mode`、检查 `buffers_body` 冲突、记录 codec 与 provider 的交集；失败保留 LKG。
- `HttpState` 通过 `nico-http` 定义的字节级 `LlmRoutes` trait 持有一份发布值；`nico-http` 不依赖 `nico-llm`。发布期对每个 convert 路由构建 `Arc<Exchange>`（codec/adapter 共享，`Exchange` 只持有指针与能力交集）；路由匹配后数据面克隆该指针，`Exchange::prepare` 产生请求自己的值。`mode: passthrough` 的路由不构建 `Exchange`，只发布字节直通所需的目的地与协议事实。
- 未知 protocol/provider kind 是发布失败，不是请求期 500；`mode: passthrough` 要求 codec.protocol 与 provider 原生协议一致，否则发布失败。

## 6. 数据面交换

### 6.1 请求

```text
request_filter: global → route match → route request plugins
request_body_filter: hold every chunk (convert) / stream (passthrough)
EOS: Exchange::prepare
       Provider::endpoint(route)          # 只读路由；也可在 body 之前单独调用
       ClientCodec::decode_request once   # 有界缓冲的 body
       ChatRequest::validate → capability gate → ProviderRoute::model
       Provider::encode(route, &request)  # wire body
     data plane sends POST WireRequest { endpoint, body }; response head/body follow
```

- convert 路由在 `request_body_filter` 里把 chunk 扣住并累加；上限 `request.max_body_bytes`，超限 413。EOS 后只调用一次 `decode_request`，canonical 请求可重放。
- `RouteTarget::Provider` + `mode: convert` 是 **AI route mode**，与插件 `buffers_body` 是两件事；同一路由同时出现时发布期拒绝，避免两套 hold-back 语义。
- passthrough 路由不构建 `Exchange`：客户端协议等于 provider 原生协议，`request_body_filter` 直接转发字节，canonical decode 与 capability gate 都不参与。
- 上游请求头由 provider 的 `endpoint()` 提供（path/headers/destination），在 body 解码前即可确定；请求体在 EOS 后用 chunked 写出（移除客户端 `content-length`，由数据面重新帧化）。固定 endpoint 的 provider 走这条路；Gemini 的模型在 path 里，见 §10。
- LLM 路由的请求插件只支持头阶段（认证/限流）；body 阶段插件在 1.4a 不支持，发布期拒绝。日志阶段照常。

### 6.2 响应

`nico-http` 定义协议无关的 per-request transform：

```rust
pub trait ResponseTransform: Send {
    fn head(&mut self, provider: &ResponseFacts) -> ResponseFacts; // 客户端状态/头
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<u8>, HttpError>;
    fn finish(&mut self) -> Result<Vec<u8>, HttpError>;
}
```

- LLM 路由在响应头钩子安装 transform，在 `upstream_response_body_filter` 逐块调用；`response_filter` 仍负责 1xx 保护与预算复核。
- SSE：每个 provider chunk 交给 `ProviderStream`，产出的 canonical 事件由 `ClientCodec` 编码为客户端 SSE；`content-length` 永远不保留，数据面走 chunked。
- 非流式：`push` 在 `request.max_response_bytes` 内累积，`finish` 时 `decode_response` → `encode_response`；超限 502 + `upstream_response_too_large`。
- provider 非 2xx：`Provider::decode_response` 返回 `ProviderError::Upstream { error }`，`Exchange::respond` 用 `ClientCodec::encode_error` 生成客户端错误体；不转发 provider body。
- 成功响应：`Exchange::respond` 返回 codec 的 `ClientReply`，状态由 codec 决定（`200`），不是 provider 的 2xx。`Exchange` 只负责 `decode_response`→`validate`→`encode_response`，不拥有任何状态映射。
- 首字节提交后不重试、不换 provider（与 1.2/1.3 的 committed 规则一致）。

### 6.3 帧化与预算

- 请求：provider 请求体长度可能变化，一律移除客户端 `content-length`，由数据面写 `transfer-encoding: chunked`；这与现有 buffered 插件模式同一套机制。
- 响应：响应体转换必然改变长度，删除 provider `content-length`；SSE 走 chunked。
- 整个 AI 交换仍由请求剩余预算包住；body 缓冲和响应转换各自检查预算（408/504 的既有边界）。

## 7. 错误

| 来源 | canonical | 客户端 | 说明 |
|---|---|---|---|
| 客户端 body 超限 | — | 413 | 数据面在 decode 前拒绝 |
| 不支持的 protocol/provider kind | — | 发布失败 | 未知注册项不进入请求路径；保留 LKG |
| 请求需要的能力超出交集 | `CanonicalError`（`unsupported_capability`） | 422 | 发布期记录交集；请求体声明的需求在 `Exchange::prepare` 拒绝 |
| 客户端文档不合法 | `CanonicalError`（`invalid_request`） | 400 | codec 的 `InvalidRequest`；诊断只进日志 |
| 网关编码/canonical 不变量失败 | `CanonicalError`（`client_encode_failed` / `provider_encode_failed` / `internal_error`） | 500 | 网关缺陷；诊断只进日志 |
| provider 4xx/5xx | `CanonicalError`（类别/稳定 code） | 客户端协议错误体 | 不泄露 provider body |
| provider 连不上/超时 | `Upstream`/`Deadline` | 502/504 | 沿用 `HttpError` 映射 |
| 首字节后断流 | `Cancelled`/`Upstream` | 不补造 | 结束流并记录 |

`CanonicalError` 不携带 provider 原始 body；provider adapter 可以记录 provider code/状态到结构化日志，但不进响应。

## 8. 观测

- 发布期：每个 protocol/provider 一行 `info`（kind、能力、版本）；每个 LLM 路由一行 `info`（protocol、provider、mode、model_map 命中）。
- 请求期：`llm.protocol`、`llm.provider`、`llm.model`（逻辑 → 物理）、`llm.stream`、`config.generation`、`http.status`、`elapsed_ms`。
- 不记录 prompt、response body、`Authorization`、provider key；`CanonicalError` 的 message 只保留安全文本。
- `Endpoint`、`WireRequest`、`RequestFacts`、`ResponseFacts`、`PreparedExchange`、`ClientReply` 的 `Debug` 只手写 header 名、长度与模型摘要：凭证、prompt 与响应体没有 Debug 拼写。

## 9. 测试

- `schema/`：每个形状的 golden JSON fixture、构造拒绝、未知字段保留；`RawJson` 不越界。
- OpenAI codec：请求/响应/流式的语义 round-trip；SSE 任意分片/跨块 UTF-8/多行 data/心跳；错误编码。
- OpenAI provider：mock upstream 断言 endpoint/headers/body；response/stream 的 canonical 事件；凭证注入与模型映射。
- 端到端：OpenAI client → OpenAI provider（convert）与 passthrough；8 MiB 分片；SSE 200 ms 节奏；首字节后不重试；413/502 边界。
- 结构：`schema/`、`protocol.rs`、`provider.rs`、`provider/route.rs`、`filter.rs`、`stream/` 无 vendor 名/switch；`cargo tree -p nico-http | grep nico-llm` 无输出；`request.max_response_bytes` 只能收紧。
- 矩阵 harness：`(protocol, provider)` 的 fixture 入口在 1.4a 建好，先跑 OpenAI×OpenAI；1.4b 填 Anthropic/Responses/Gemini。

## 10. 风险与已知缺口

| 风险 / 缺口 | 处置 |
|---|---|
| convert 路由请求体缓冲 = `request.max_body_bytes` × 并发 | 与 buffered 插件同一算术；文档写明，passthrough 不缓冲 |
| 非流式响应缓冲 = `request.max_response_bytes` × 并发 | 新字段，默认 8 MiB，路由只能收紧；SSE 不适用 |
| Gemini endpoint 的 model 在 path 里，而 model 在请求 body 里 | **1.4b 专门设计**：要么路由/provider 固定物理模型（body model 只校验），要么实现 pre-body exchange（需验证 Pingora 能否在 upstream head 前读 body）；1.4a 不假装已解决 |
| canonical 无法表达某些厂商字段 | `extensions` + `RawJson` + capability matrix；不支持的能力显式报错；raw passthrough 兜底 |
| SSE 转换状态跨块 | 每响应一个 `ProviderStream`；不放在 `Provider` 单例或 `Plugin` 里 |
| LLM 路由的 body/response 插件 | 1.4a 发布期拒绝 body 阶段插件；头阶段认证/限流不受影响 |
| 流式转换的背压/慢客户端 | 沿用 1.2 的写路径与预算；transform 不另建无界队列 |
| TLS | 1.4a 用明文 mock 验收；真实 https provider 仍需独立 TLS 任务 |
| 两套 canonical（Chat vs Responses） | canonical `Item` 统一承载；Responses codec 在 1.4b 映射到同一形状，不引入第二套模型 |

## 11. 交付边界

- **`nico-llm`**：`schema/`、`protocol/openai_chat.rs`、`provider/{route,openai}.rs`、`stream/sse.rs`、`filter.rs` + `filter/{error,stream}.rs`；`builtin_registry()` 注册 codec/provider。
- **`nico-http`**：字节级 `LlmRoutes`/`ResponseTransform` trait、AI route mode、响应头映射、请求 hold-back 复用；不 import `nico-llm`。
- **`nico-core`**：`routes[].protocol`/`mode`、`request.max_response_bytes`、`providers[].model_map`；只做文档模型/校验，不含厂商语义。
- **`nico-server`**：注册 codec/provider、编译 `LlmPlan`、与快照同一发布；日志。
- **spec**：`directory-structure.md` 的 `nico-llm` 模块树；“无 vendor switch”的结构规则；`pingora-guidelines.md` 的 AI 缓冲/转换；`logging-guidelines.md` 的 `llm.*`；`error-handling.md` 的 canonical error。

## 12. Bifrost 参考

Bifrost 把 `core/schemas` 作为 canonical（`BifrostChatRequest`/`BifrostChatResponse`/`BifrostStreamChunk`），`core/providers/<vendor>` 实现 `Provider` 接口并做 wire ↔ canonical，`transports/bifrost-http/integrations` 做客户端协议 ↔ canonical。nico 采用同样的两轴分层，但不照搬它的 OpenAI 形状：

- 保留：canonical 形状、provider 接口、客户端 integration 与 provider adapter 分离、raw 请求逃生口、fallback 可重放。
- 改进：canonical 用 `Item`/`StreamEvent`，不把 OpenAI chunk 当中间格式；`Provider` 只做编码与 endpoint，不拥有 HTTP/TLS；能力矩阵显式化。
- 细节见 `.trellis/tasks/11-04-s1-4a-nico-llm-model/research/bifrost-protocol-model.md`。
