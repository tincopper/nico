# Bifrost 的 LLM 协议模型（参考实现）

> 目的：为 nico 的 canonical 模型与 codec/provider 分层提供一手参考。代码位置：`/Users/tomgs/workspaces/go_space/bifrost`（Go）。只记录结构与可迁移的结论，不复述其业务功能。

## 1. 三层结构

| 层 | 位置 | 职责 |
|---|---|---|
| canonical | `core/schemas/` | `BifrostChatRequest` / `BifrostChatResponse` / `BifrostStreamChunk`；`BifrostRequest` 是各能力的 oneof；消息、内容块、工具、usage 都在这里 |
| 客户端协议 | `transports/bifrost-http/integrations/` | `/v1/chat/completions`、`/v1/messages` 等入口：客户端 wire ↔ canonical |
| 上游厂商 | `core/providers/<vendor>/` | 厂商 wire ↔ canonical；实现 `Provider` 接口 |

关键文件：

- `core/schemas/chatcompletions.go:16` `BifrostChatRequest`（`Provider`/`Model`/`Input []ChatMessage`/`Params`/`Fallbacks`/`RawRequestBody`）。
- `core/schemas/chatcompletions.go:38` `BifrostChatResponse`；`:1117` `ChatMessage`；`:1250` `ChatMessageContent`；`:1304` `ChatContentBlockType`；`:1316` `ChatContentBlock`。
- `core/schemas/bifrost.go:2193` `BifrostStreamChunk`；`:821` `BifrostRequest`（每种能力一个字段的 oneof）。
- `core/schemas/provider.go:789` `Provider` 接口：`ChatCompletion(ctx, key, *BifrostChatRequest) (*BifrostChatResponse, *BifrostError)`、`ChatCompletionStream(...) (chan *BifrostStreamChunk, *BifrostError)`、`Responses`/`ResponsesStream` 等。
- `core/providers/anthropic/chat.go:347` `ToAnthropicChatRequest`（canonical → 厂商）；`:1224` `ToBifrostChatResponse`（厂商 → canonical）；`:1505` `ToAnthropicChatResponse`（canonical → 客户端 Anthropic）；`:1736` `ToBifrostChatCompletionStream`（厂商流 → canonical）。
- `transports/bifrost-http/integrations/anthropic.go:124` `/v1/messages`：Anthropic 客户端协议入口，与 provider 适配分开。
- `BifrostChatRequest.RawRequestBody`：纯透传逃生口；`Fallbacks`：canonical 可重放，失败时重新编码。

## 2. 两个独立轴

Bifrost 支持的不只是“OpenAI 客户端 → 多厂商”：

- 客户端 integration 可以把 Anthropic `/v1/messages` 解成 canonical，再由 OpenAI provider 编码；
- provider adapter 的 `ToAnthropicChatResponse` 说明 canonical 也能编回 Anthropic 客户端格式。

所以真正的维度是：
```text
客户端协议 (N)  ×  上游 provider (M)
            \        /
             canonical
```
没有 canonical 就是 N×M 个分支；有 canonical 就是 N 个 codec + M 个 provider adapter，核心只认 canonical。

## 3. 可迁移的做法

1. **canonical 是唯一的公共词汇**：provider 接口的签名只出现 canonical 类型，不出现厂商 JSON。
2. **客户端协议与 provider 分开**：`transports/.../integrations` 与 `core/providers` 是两个包；新增客户端协议不碰 provider，新增厂商不碰 integration。
3. **每个 provider 只做 wire 转换**：请求/响应/流/错误四个方向都在 adapter 内；核心没有 `switch provider`。
4. **raw 逃生口**：`RawRequestBody` 用于纯透传；convert 与 passthrough 是两种明确模式。
5. **canonical 可重放**：`Fallbacks` 存在请求对象上，重试/换 provider 时重新编码，不依赖原始 body 重放。
6. **流式是 per-response 状态**：`ChatCompletionStream` 返回 channel，流解析状态在流实例内，不在 provider 单例。

## 4. 不照搬的地方

1. **OpenAI 形状当 canonical**：`BifrostChatRequest` 的 `Input []ChatMessage` 与 OpenAI messages 很接近，另有独立的 `BifrostResponsesRequest`。nico 的 canonical 用 `Item`（message/tool call/tool result/reasoning）与 `StreamEvent`，让 Responses/Anthropic/Gemini 都映射到同一组形状，避免两套 canonical。
2. **巨型 `BifrostRequest` oneof + 30+ 方法的 `Provider`**：Bifrost 覆盖 chat/responses/embedding/speech/image/video/batch/...；nico 阶段 1 只需要 chat + stream，先小接口，按真实能力扩展。
3. **provider 自己发 HTTP**：Bifrost 的 provider 在 adapter 内完成网络调用；nico 的连接、TLS、预算、重定向和帧化归 `nico-http`，provider 只产出 `Endpoint`/`WireRequest`，便于统一的预算与安全策略。
4. **canonical 里放 `Provider` 字段**：Bifrost 的 SDK 允许显式选 provider；nico 由路由/配置选 provider，canonical 只带客户端写的 logical model，物理模型由 `model_map` 解析，避免把路由决策塞进数据形状。

## 5. 对 nico 的结论

- `nico-llm` 用 `schema/`（canonical）+ `protocol/`（客户端 codec）+ `provider/`（厂商 adapter）+ `filter.rs`（交换编排）。
- `Provider` 接口只接受/返回 canonical；`ProviderStream` 每响应新建。
- 1.4a 先冻结形状与 OpenAI；1.4b 按 `(protocol, provider)` 矩阵补 Anthropic/Responses/Gemini。
- 形状里的 `extensions`/`RawJson` 与 `mode: passthrough` 是“canonical 不能表达一切”的诚实出口；能力矩阵负责显式拒绝，而不是静默丢字段。

## 6. 已知的传输差异

Bifrost 的 HTTP server 在读完整请求后转发，所以 provider endpoint 可以依赖 body（例如 Gemini 的 model 在 path）。Pingora 是 head-first 流式代理；Gemini 的 `{model}` 在 path、model 在 body，是 nico 1.4b 必须单独解决的传输问题（固定路由物理模型，或 pre-body exchange）。1.4a 只承诺固定 endpoint 的 OpenAI/Anthropic/Responses。
