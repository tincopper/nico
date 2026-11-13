# 调研：异构 Provider 到底指什么

## 1. 问题从哪来

`11-03-unified-gateway-analysis/implement.md` §3 把「异构 Provider（非 HTTP/JSON 原生、或需要本地模型服务器语义的
kind）」列为阶段 3 未交付项，并注明「语义待澄清」。本文件澄清它：这不是"再加一个 adapter"的工作量，
而是对现有 `Provider` 契约的一次边界测试。

**方法**：仓库内部事实从源码核实（§2、§3）；候选语义的**外部事实**（Ollama / vLLM / llama.cpp / TGI
的 API 形状）因本次环境无法访问官方文档，标为「待核实假设」，不写成结论（§4）。

## 2. 现有契约的硬边界（已核实）

| 位置 | 事实 | 推论 |
|---|---|---|
| `nico-core/src/config/upstream.rs:98` | `Scheme` 只有 `Http` / `Https` | 数据面只会开 TCP + 可选 TLS |
| `nico-llm/src/provider/endpoint.rs:33` | `base_url` 必须是绝对 `http(s)` URL；拒绝未知 scheme、userinfo、query、fragment、空 host | 非 HTTP scheme（`grpc://`、`unix://`、进程内）在解析期就被拒 |
| `nico-llm/src/provider/endpoint.rs:91` | `Endpoint = Destination + path + headers + model` | 一次调用 = 一个 HTTP 请求，带固定 path 与 headers |
| `nico-llm/src/provider.rs:192` | `Provider::endpoint` 在**发布期**编译一次，`native_protocol()` 决定 passthrough 是否合法 | 端点形状是编译期常量，不是请求期决策 |
| `nico-http/src/llm/attempt.rs:190` | `LlmAttempt::select -> AttemptTarget`（destination/path/headers/body） | 数据面只认这四件套；`upstream_peer` 用它建 `HttpPeer` |
| `nico-llm/src/lib.rs` | `builtin_registry()` 装配四个 kind（openai / openai_responses / anthropic / gemini） | 四个都是远程 HTTP/JSON API，方言不同但**传输与调用形状相同** |

**结论 A**：现有抽象统一的是「**HTTP 上的 JSON 方言**」，不是「任意 provider」。四种 codec × 四种 adapter
的矩阵之所以成立，是因为它们共享同一个 `Endpoint` 形状。异构不是一个新 kind，而是这个共享前提失效。

**结论 B**：`passthrough` 已经证明契约能容忍"不解码"——`PassthroughRoute` 不碰 canonical model，但
它**仍然走 `Endpoint`**（仍是一个 HTTP 请求）。所以"不转换"≠"非 HTTP"，两者不能混为一谈。

## 3. 「异构」至少是三种不同的需求

把它们分开是本次调研最重要的产出。它们的改动面差一个数量级，混成一个词会让设计无从下手。

### 3.1 方言异构（已解决，无需工作）

Ollama / vLLM / llama.cpp / TGI 等本地推理服务器**多数提供 OpenAI-compatible 的
`/v1/chat/completions`**（待核实假设，见 §4）。若属实，它们用现有 `kind: openai` +
不同的 `base_url` 就能服务，只需要：

- 一个 `model_map` 把调用方的逻辑名映射到本地模型名；
- 可能不需要 `credential_ref`（现有 schema 是否允许缺省待核实）；
- 一个 `base_url` 指向 `http://127.0.0.1:11434` 之类。

**这是配置，不是代码。** 若这条路成立，roadmap 所说的「需要本地模型服务器语义」里，绝大部分
需求已经由现有契约覆盖，缺的只是文档与一个可用的样例。

### 3.2 协议异构（中等改动）

同一个 HTTP 传输，但 wire 不是 JSON-RPC 风格的请求/响应：例如 gRPC（HTTP/2 + protobuf）、
WebSocket 流式、SSE-only 的上行、或需要 multipart 上传的二进制协议。

改动面：
- `Endpoint` 需要能表达非 JSON body 的 framing（method 之外还有 content-type、stream framing）；
- `Provider::encode` 已从 `ChatRequest` 产出 `Vec<u8>`，**这一层其实已经不关心 JSON**——它是字节；
- 但 `upstream_request_filter` 里的 `host`/`x-forwarded-*` 与 Pingora 的 `HttpPeer` 假设 HTTP/1.1
  请求头，gRPC 需要 ALPN H2（`upstream.rs` 的注释已写明 `is_default_port` 与 ALPN H1 默认值）。

**判断**：这更像"给 `Endpoint` 增加一个 `alpn` / framing 维度"，而不是引入新 trait。第二个真实消费者
出现前不做（spec 的 second-consumer 规则）。

### 3.3 传输异构（大改动）

`grpc://`、`unix://`、进程内库调用、或需要长连接会话池的协议。

改动面：
- `Scheme` 要扩展（破坏性：`Destination` 在 `nico-core` 公开，很多 crate 依赖）；
- `upstream::peer()` 只建 `HttpPeer`，非 HTTP 传输需要另一套 peer 构造；
- `AttemptTarget` 的 `destination/path/headers` 三件套不再有意义，需要一个新的 seam；
- Pingora 本身是 HTTP 代理框架，非 HTTP 上游实际上超出它的能力范围。

**判断**：这**不是本项目的扩展**，而是一个不同的产品。roadmap §4 已经写了正确的处理方式：
「新协议使用独立 crate/binary 和协议原生 trait，不穿透 HTTP Filter」。

## 4. 待核实假设（不写成结论）

本次环境无法访问官方文档（`WebFetch` 超时、`WebSearch` 不可用），以下需要在有网络时验证：

1. **Ollama / vLLM / llama.cpp / TGI 是否都提供 OpenAI-compatible `/v1/chat/completions`**，
   以及它们的流式 SSE 是否与 OpenAI 的 `data: {...}\n\n` + `[DONE]` 字节兼容。若兼容，§3.1
   的结论成立；若不兼容（例如 Ollama 原生 `/api/chat` 的 NDJSON 形状），则需要一个**新的
   provider kind**，但它仍是 HTTP/JSON，落在 §3.2 而不是 §3.3。

2. **本地模型服务器是否需要"模型列表/预热/加载"这类非请求期语义**：若需要，那是发布期
   或生命周期语义，与 `Provider::endpoint` 的"编译一次"假设冲突，需要单独设计。

### 4.2 已核实（不是假设）

`credential_ref` 在 `nico-core/src/config/raw.rs:56` 是必填 `String`，语义层
（`semantic.rs:157`）要求它过 `is_secret_ref`。**本地推理服务器没有 key，但文档必须写一个
`env:` / `file:` 引用**，实际值可以是占位符（如 `env:NICO_UNUSED`）。这是配置上的 workaround，
不需要改代码；但它值得在文档里写明，否则一个部署者会以为网关强制要求密钥。

## 5. 决策建议

按 spec 的「先定不变量与失败模式，再选结构」与「没有第二个真实消费者不抽 trait」：

| 建议 | 理由 |
|---|---|
| **把 §3.1 收口为文档 + 样例，不写代码** | 现有 `kind: openai` + `base_url` 已能表达；写一个新的 provider kind 会造出第二个只做同一件事的实现 |
| **把 §3.2 记为"待第二个消费者"** | 目前只有 JSON 方言；`Endpoint` 加 framing 维度是真实扩展点，但现在加是猜测 |
| **把 §3.3 明确排除，写进 roadmap** | 它违反 Pingora 的能力边界，答案在 §4 已有的「独立 crate/binary」 |
| **在动手前先验证 §4 的三条假设** | 特别是第 1 条：它决定「本地模型服务器」到底是一个配置样例，还是一个真需求 |

## 6. 对现有 roadmap 条目的修订建议

把
> 异构 Provider（非 HTTP/JSON 原生、或需要本地模型服务器语义的 kind）：语义待澄清。

改成三行，分别对应 §3.1/3.2/3.3，并把 §3.3 标为 Out of Scope。这样「语义待澄清」这个阻塞
就被拆成了「已覆盖 / 待第二个消费者 / 明确不做」三类，每一类都可独立决策。
