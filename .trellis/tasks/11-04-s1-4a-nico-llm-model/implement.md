# 1.4a 实施计划：先形状，后逻辑

> 设计见 `design.md`。顺序是硬约束：`schema/` 冻结之前不写 codec/provider；两条数据面缝验证之前不做端到端验收。

## 1. canonical 形状（无 I/O）

- [ ] 建立 `crates/nico-llm` 骨架：`lib.rs`、`schema.rs` + `schema/{request,conversation,tool,response,stream,usage,error,model}.rs`；`#![deny(missing_docs)]`，文件各守一个数据边界。
- [ ] 形状与构造校验：`ChatRequest`/`Item`/`ContentPart`/`Tool`/`ChatResponse`/`StreamEvent`/`Usage`/`CanonicalError`/`ModelRef`；`RawJson` 只允许 tool arguments 与 `Extensions`。
- [ ] golden JSON fixtures 与 round-trip/拒绝测试；未知字段进 `extensions`，不静默丢。
- [ ] **检查点 A**：`cargo test -p nico-llm`，schema 无 `protocol/provider/http` 依赖；文件 ≤500 行。

## 2. 契约与注册表

- [ ] `protocol.rs`：`ClientCodec`/`ProtocolRegistry`/`Capabilities`/`CodecError`；`provider.rs`：`Provider`/`ProviderStream`/`ProviderRegistry`/`ProviderError`/`Endpoint`/`WireRequest`。
- [ ] `filter.rs`：路由交换的编排（decode → endpoint/encode → response/stream），错误映射到 `CanonicalError`。
- [ ] **检查点 B**：mock codec/provider 的单元测试；核心无 vendor 名（结构测试先红后绿）。

## 3. OpenAI Chat codec

- [ ] `protocol/openai_chat.rs`：请求 decode（messages/tools/sampling/response_format/stream）；响应/流式 encode；错误 encode。
- [ ] SSE 分片安全：跨块 UTF-8、多行 `data:`、`[DONE]`、心跳；未知字段保留。
- [ ] **检查点 C**：请求/响应/流式的语义 round-trip；任意分片测试。

## 4. OpenAI provider

- [ ] `provider/openai.rs`：`endpoint`（base_url/固定 path/headers）、`encode`（canonical → OpenAI body）、`decode_response`、`stream_decoder`。
- [ ] 凭证发布期 materialize；`model_map` 逻辑→物理；provider 错误 → `CanonicalError`；不泄露 provider body/凭证。
- [ ] **检查点 D**：mock upstream 断言 path/headers/body、响应/流式事件；模型映射与错误映射。

## 5. 数据面缝

- [ ] `nico-core`：`routes[].protocol`（provider 路由必填）、`routes[].mode`（默认 convert）、`providers[].model_map`、`request.max_response_bytes`（默认 8 MiB，路由只能收紧）。
- [ ] `nico-http`：字节级 `LlmRoutes`/`ResponseTransform` trait；AI route mode 的 hold-back 复用；`upstream_response_filter` 的头映射；`upstream_response_body_filter` 的 transform；`HttpState` 发布 `LlmPlan`。
- [ ] 发布期校验：未知 protocol/provider、`mode: passthrough` 与原生协议不匹配、`buffers_body` 与 AI 路由冲突、LLM 路由的 body 阶段插件，全部拒绝并保 LKG。
- [ ] **检查点 E**：`OpenAI client → OpenAI provider` convert + passthrough 端到端；SSE 逐块；413/502；首字节后不重试。

## 6. 观测、文档与门禁

- [ ] 日志字段：`llm.protocol`/`llm.provider`/`llm.model`/`llm.stream`；发布期 protocol/provider 行；不记 prompt/body/凭证。
- [ ] spec：`directory-structure.md` 的 `nico-llm` 模块树与“无 vendor switch”规则；`pingora-guidelines.md` 的 AI 缓冲/转换；`logging-guidelines.md` 的 `llm.*`；`error-handling.md` 的 canonical error。
- [ ] `implement.jsonl`/`check.jsonl` 包含 design、research、相关 spec。
- [ ] **门 A**：`cargo +nightly fmt --all --check`、`cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`、`cargo test --workspace --all-features`、`cargo deny check`、`RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`。
- [ ] **门 B**：依赖边界（`nico-http` 无 `nico-llm`；`nico-core` 无网络/密码学）、文件预算、`(protocol, provider)` matrix harness 可运行、无 vendor switch 结构测试。

## 回滚点

- 第 1 步（schema）独立提交：只增类型/测试，可单独回退。
- 第 3/4 步（codec/provider）不触碰数据面，可单独回退。
- 第 5 步（数据面缝）是行为变更，单独提交；回退即恢复 provider 路由拒绝 + 无 AI 缓冲。
- 每一步都在 `implement.md` 勾选并记录门禁输出；红的门不绕过（`SKIP_NICO_HOOKS` 仅工具故障）。
