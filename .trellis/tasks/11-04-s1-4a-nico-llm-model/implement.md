# 1.4a 实施计划：先形状，后逻辑

> 设计见 `design.md`。顺序是硬约束：`schema/` 冻结之前不写 codec/provider；两条数据面缝验证之前不做端到端验收。

## 1. canonical 形状（无 I/O）

- [x] 建立 `crates/nico-llm` 骨架：`lib.rs`、`schema.rs` + `schema/{request,conversation,tool,response,stream,usage,error,model}.rs`；`#![deny(missing_docs)]`，文件各守一个数据边界。
- [x] 形状与构造校验：`ChatRequest`/`Item`/`ContentPart`/`Tool`/`ChatResponse`/`StreamEvent`/`Usage`/`CanonicalError`/`ModelRef`；`RawJson` 只允许调用方声明的 JSON 文档（tool parameters、tool arguments、structured-output schema）与 `Extensions`。
- [x] golden JSON fixtures 与 round-trip/拒绝测试；未知字段进 `extensions`，不静默丢。
- [x] **检查点 A**：`cargo test -p nico-llm`，schema 无 `protocol/provider/http` 依赖；文件 ≤500 行。

  **落地记录（第 1 步）**
  - 新增 `crates/nico-llm`：`Cargo.toml`、`src/lib.rs`、`src/schema.rs`，以及 `schema/{request,conversation,tool,extension,response,stream,usage,error,model}.rs`。`schema/extension.rs` 是按数据边界拆出的文件（`RawJson` + `Extensions`），因为把两者和 tool 形状放在一起触到 500 行硬上限。
  - 公开形状：`ChatRequest`/`Sampling`/`ResponseFormat`/`JsonSchema`；`Item`/`Message`/`Role`/`ContentPart`/`ImageSource`/`Reasoning`；`Tool`/`ToolCall`/`ToolResult`/`ToolChoice`/`RawJson`/`Extensions`；`ChatResponse`/`Choice`/`AssistantTurn`/`FinishReason`/`ResponseId`；`StreamEvent`；`Usage`/`UsageCounts`/`TokenKind`；`CanonicalError`/`SchemaError`；`ModelId`/`ModelRef`。字段私有 + 构造器/访问器；公开枚举 `#[non_exhaustive]`。
  - `SchemaError` 覆盖：空必填、长度上限、非有限/越界数值、非对象/空对象、无效 JSON、重复名/重复 index、未声明 tool、空 conversation/content/choices、空/保留 extension key。回显的请求文本统一截断为诊断摘要（≤64 字节、按字符边界），所以被拒绝的 `data:` URL 或超长字段不会把请求体带进日志；`Invalid`/`Duplicate`/`UndeclaredTool` 的字段文档写明是 excerpt。
  - `Role` 只有 system/developer/user/assistant：tool 答案是 `Item::ToolResult`（携带所属 call id），codec 把 wire 的 `role=tool` 映射到该 item。没有保留一个永远被构造器拒绝的公开 `tool` 变体——那会让每个 codec 面对同一事实的两个拼写；测试断言 `"role":"tool"` 在 serde 层即被拒绝。
  - `schema/` 的文档与示例不点名厂商：结构规则要求 vendor 名只出现在 `protocol/<name>` 与 `provider/<name>`，所以协议对比改为“某个厂商的 `messages[]`/chunk”“delta 数组、content-block delta、output-text 事件、candidate part”。crate 根 `lib.rs` 仍说明它桥接的四个协议（这是 crate 的用途，不是 schema 的知识）；tests/fixtures 里的模型字符串只是数据，不参与结构判定。
  - fixtures：`tests/fixtures/{chat_request,chat_response,stream_events}.json`；`tests/schema_round_trip.rs`（8 tests）+ `tests/schema_rejects.rs`（20 tests，请求/响应树）+ `tests/schema_values.rs`（9 tests，叶子值），共 37 个测试。两个拒绝测试文件按数据边界拆开，各自远低于 500 行。请求/响应顶层未知字段经 `#[serde(flatten)] extensions` 保留，并断言重新序列化仍在；嵌套未知字段由 codec 提升到 enclosing `extensions` 或显式拒绝（`schema/extension.rs` 写明 serde 形态只捕获顶层未知字段）。
  - `nico-core`：仅给 `ErrorCategory` 加 `Serialize`/`Deserialize` + `snake_case`，供 `CanonicalError` 的 fixture/诊断表示使用；无网络/密码学依赖。
  - 门（真实输出）：`cargo test -p nico-llm` 37 passed（5 suites）；`cargo clippy -p nico-llm --all-targets --all-features -- -D warnings` 无 issue；`cargo +nightly fmt --all --check` 通过；`cargo check --workspace --all-features` 通过；`RUSTDOCFLAGS="-D warnings" cargo doc -p nico-llm --no-deps` 通过；`cargo test --workspace --all-features` 720 passed（36 suites）。
  - 偏离/落地决定：`Sampling` 的 `temperature`/`top_p` 用 `f64`（`f32` 的 `0.2` 会序列化为 `0.20000000298023224`，golden round-trip 无法相等）；枚举统一使用 adjacently-tagged serde（`{"type":…,"data":…}`），`ContentPart::File` 按 design 保留 struct variant；`JsonSchema` 作为 `ResponseFormat::JsonSchema` 的已校验载荷单独导出。`RawJson` 的领域例外（caller-supplied JSON documents + unmodelled fields）写在 `schema/extension.rs` 模块文档，配置/路由/插件仍禁用 `serde_json::Value`。

## 2. 契约与注册表

- [x] `protocol.rs`：`ClientCodec`/`ProtocolRegistry`/`Capabilities`/`CodecError`；`provider.rs`：`Provider`/`ProviderStream`/`ProviderRegistry`/`ProviderError`/`Endpoint`/`WireRequest`。
- [x] `filter.rs`：路由交换的编排（endpoint → decode → encode → response/stream），错误映射到 `CanonicalError`。
- [x] **检查点 B**：mock codec/provider 的单元测试；核心无 vendor 名（结构测试先红后绿）。

  **落地记录（第 2 步）**
  - 新增 `src/protocol.rs`（424 行）、`src/protocol/facts.rs`（148）、`src/provider.rs`（443）、`src/provider/route.rs`（243）、`src/filter.rs`（292）与 `src/filter/{error,stream}.rs`（146/55）；`lib.rs` 导出四个模块与公开契约；`Cargo.toml` 加 `nico-http`（只为 `Destination`），无其他新依赖。
  - `ClientCodec`（`protocol`/`capabilities`/`decode_request`/`encode_response`/`encode_stream_event`/`encode_error`）与 `Provider`（`kind`/`capabilities`/`endpoint`/`encode`/`decode_response`/`stream_decoder`）都是 object-safe + `Send + Sync`；`ProviderStream` 每响应新建、`Send`。`ProtocolRegistry`/`ProviderRegistry` 是 `BTreeMap<&'static str, Arc<dyn …>>`，重复 kind 返回共享的 `RegistryError::DuplicateKind`，`Debug` 只打印 kind。
  - `ProtocolId`/`ProviderKind` 是 `&'static str` newtype（`as_str`/`Display`，与 `PluginFactory::kind()` 同风格）：不能用 provider kind 去索引 codec 注册表。
  - `Capabilities` 不引入新依赖：`Capability` 枚举 + `u8` 位集（`supports`/`with`/`without`/`intersection`/`iter`/`is_empty`），`Capability::ALL` 的位序由 `const _: () = assert!(CAPABILITY_COUNT <= u8::BITS as usize)` 守住；`name()` 是日志与错误里的稳定拼写。
  - `RequestFacts`/`ResponseFacts` 落在 `protocol/facts.rs`（`protocol.rs` re-export，公开路径不变）：owned method/path/headers/status，`header()` 大小写不敏感且取第一个值，不含 Pingora 类型。codec 读 request facts，provider 读 response facts，`filter.rs` 两者都用。两者的 `Debug` 都只手写 header 名，不打印值（值可能是客户端或上游的凭证）。
  - `CodecError`（`InvalidRequest`/`Unsupported{feature: Capability}`/`Encode`）与 `ProviderError`（`Unsupported`/`InvalidRequest`/`InvalidEndpoint{source: EndpointError}`/`Malformed`/`Upstream{error: CanonicalError}`）中的 `reason` 文档写明仅供日志、不含 body 或凭证；provider 的失败状态只经 `Upstream` 的 `CanonicalError` 出现。`Unsupported` 只用于请求期（decode/prepare）；响应/流编码失败一律 `Encode`，provider 响应无法表达用 `Malformed`，所以 422 不会出现在上游已经答复之后。
  - `ProviderRoute`（`route_id`/`protocol`/`provider_kind`/`base_url`/`credential`/`model_map: BTreeMap<ModelId, ModelId>`/`mode`）在发布期构建：`try_new` 拒绝空 id/base_url，`with_model_map` 把两侧都解析成 `ModelId`，所以 `model()` 在请求期是不会失败的查找；`Debug` 手写，credential 只打印 `true/false`；`Credential` 的 `Debug` 恒为 `Credential(<redacted>)`，读值必须显式 `expose()`。
  - `Endpoint::try_new` 校验 path 绝对且无控制字符/空格、header name 是 HTTP token、value 无控制字符：`file:` 读进来的凭证常带换行，这一层把“凭证里的换行拆出第二个请求”变成 `EndpointError`。`WireRequest` 只是 endpoint + body；method 与帧化归数据面（chat 交换恒为 POST），`Endpoint` 按任务要求不带 method。`Endpoint`/`WireRequest`/`PreparedExchange`/`ClientReply` 的 `Debug` 手写：只打印 header 名、body 长度与模型摘要，不打印 Authorization、prompt 或响应体。
  - `Exchange` 只做 convert：`try_new` 拒绝 protocol/kind 不符与 passthrough 路由（passthrough 没有 canonical 交换）；`capabilities()` 是发布期交集；`endpoint()` 只读路由（测试断言 body 之前就可用）；`prepare()` = endpoint → decode 一次 → `validate()` → 能力门 → `model()` → encode，返回 `PreparedExchange{request, model, wire}`；`respond()` 把 2xx 解码后交给 codec（客户端状态由 codec 的 `ClientReply` 决定，不是 provider 的 2xx），把 `ProviderError::Upstream` 编成客户端错误体（状态取 canonical 的 `http_status`），其余错误由 `error_reply()` 统一编码；`stream()` 每次新建 `ProviderStream`，`StreamEncoder::push/finish` 逐事件交给 codec，分片边界不影响事件序列。
  - 能力门在请求期执行：`filter.rs::required()` 只遍历 canonical 模型（stream、tools、response_format、image/file/reasoning part），与交集比较后返回 `ExchangeError::Unsupported{capability}` → 422 `unsupported_capability`。这实现 PRD R3 的“工具被路由禁用”且无需 schema 知道 vendor；空交集本身不是发布失败（纯文本不需要任何 capability，design 只要求“交集并记录”）。
  - `ExchangeError::canonical()` 是全函数：新增 `CanonicalError::from_static_parts`（`pub(crate)`，公开形状不变）供网关自己的常量失败表使用；`every_failure_maps_to_a_valid_canonical_error` 对每个变体断言 `validate()` 通过且 code/status 正确。映射：`InvalidRequest` 400 / `Unsupported` 422 / `*_encode_failed`、`upstream_endpoint_invalid`、`internal_error` 500 / `upstream_protocol_error` 502 / provider 的 `Upstream` 原样透传。
  - 测试：`tests/contracts.rs`（13：capability 位集与名称、两套 facts、Endpoint 校验、ProviderRoute 模型映射与凭证脱敏/空字段拒绝、Debug 不打印 header 值、Send+Sync）＋ `tests/exchange/{main,response,stream}.rs`（21：两个注册表的重复/查询/Debug、protocol/kind/passthrough 拒绝、交集、endpoint-before-body、prepare 的物理模型与凭证注入、endpoint→decode→encode 的调用顺序、Debug 不打印 prompt/凭证/响应体、客户端错误体、streaming 与 image 能力门、成功/状态归属/失败/畸形响应、任意分片流、流失败、全部错误映射、跨 await/task 的 `Send`/`Sync`）＋ `tests/exchange/mocks.rs`（mock codec/provider/stream）＋ `tests/structure.rs`（2：`schema/`、`protocol.rs`、`provider.rs`、`provider/route.rs`、`filter.rs`、`filter/`、`stream/` 不含 `openai|anthropic|gemini|claude|gpt`；`nico-http/Cargo.toml` 不含 `nico-llm`）。共 36 个新测试。
  - 门（真实输出）：`cargo test -p nico-llm` 73 passed（8 suites）；`cargo clippy -p nico-llm --all-targets --all-features -- -D warnings` 无 issue；`cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` 无 issue；`cargo +nightly fmt --all --check` 通过；`cargo check --workspace --all-features` 通过；`RUSTDOCFLAGS="-D warnings" cargo doc -p nico-llm --no-deps` 通过；`cargo test --workspace --all-features` 757 passed（39 suites）；`cargo deny check` advisories/bans/licenses/sources 全 ok；文件预算只剩既有的 `crates/nico-config/src/cache.rs`（554，本步未触碰）；`cargo tree -p nico-http | grep nico-llm` 无输出；`cargo tree -p nico-llm | grep -E 'nico-plugins|nico-ai'` 无输出。
  - 落地决定：
    - `Provider::encode(route, request)` 接收路由：物理模型来自 `model_map`，而 provider 单例无 per-request 状态；canonical 请求不改写，重试时重新解析。
    - `Endpoint`/`WireRequest` 不带 method：字段是 destination/path/headers + body；method 恒为 POST，帧化归数据面。
    - `model.rs` 不单独建立：逻辑↔物理解析就是 `ProviderRoute::model`（编译后的 `model_map` 是路由事实）。
    - `Exchange::try_new` 不因空交集拒绝；能力不足在请求期以 `Unsupported` 拒绝，与 PRD“工具被路由禁用返回 typed error”一致。
    - 测试目录用 `tests/exchange/{main,response,stream,mocks}.rs`（`main.rs` 是 target root）：`tests/exchange.rs` 作为 crate root 时 `mod mocks` 只能指向 `tests/` 顶层，会多出一个空 test target；移动端/响应侧按场景再分子模块，文件守 500 行上限而不压缩断言。
    - `filter.rs` 292 行 + `filter/{error,stream}.rs`：Debug 脱敏把 `filter.rs` 推过 500 后，按“错误词汇”与“响应流驱动”两个边界拆开，没为行数压缩代码。
    - 本步的四处实现决定已同步进 `design.md`/`prd.md`/`directory-structure.md`：`encode(route, request)`（物理模型来自路由）、`Endpoint`/`WireRequest` 不带 method（chat 恒为 POST，归数据面）、不单独建 `model.rs`（映射是 `ProviderRoute::model`）、passthrough 路由不构建 `Exchange`；空交集不拒绝发布，请求期超出交集返回 422 `unsupported_capability`。

  **复核记录（第 2 步之后，进第 3 步之前）**

  对第 1、2 步做了一次规范符合性复核（基线五门全绿；依赖方向、无 vendor switch、panic-free、文档与测试组织均符合）。复核发现 6 项，已全部处理：

  - **`protocol.rs` 494 行，距 500 硬上限 6 行**：`RequestFacts`/`ResponseFacts` + `first_header`/`header_names` 拆到 `protocol/facts.rs`（一个数据边界：交换的事实），`protocol.rs` 只留契约、注册表与 `ClientReply`。`protocol.rs` 494→424，`facts.rs` 148；公开路径经 re-export 不变。
  - **成功状态被 provider 耦合**：`Exchange::respond` 原本用 `facts.status()` 作为客户端状态，provider 的 `201`/`206` 会漏给调用方；而“一个完整答复用什么状态”是客户端协议的性质。新增 `protocol::ClientReply { status, body }`，`ClientCodec::encode_response` 返回它，并删除 `filter::ExchangeReply`（一种形状只留一个类型，protocol 与 filter 复用）。失败状态仍归 `CanonicalError::http_status`，所以 `encode_error` 只返回 body——两个方向的归属不重叠。
  - **`ErrorCategory` 的 serde 拼写是新增契约但无测试**：在 `nico-core/src/error.rs` 加 `every_category_keeps_its_spelling`（正反双向锁 5 个拼写），并把该文件模块文档的“kernel never carries a wire-level value”改为“wire-level status”，与新增的诊断表示不再矛盾。
  - **LLM 错误状态表只写在 `design.md` 里**：`error-handling.md` 的 `LLM provider errors` 段补齐 8 行 canonical 错误表、422 相对 `Client → 400` 的理由、成功/失败状态的归属、`from_static_parts` 的合法使用边界、category 拼写的性质。
  - **`StreamEncoder::push` 的文档与行为不符**：写明按 chunk 原子——一个事件编码失败时该 chunk 已编码的字节被丢弃，调用方结束流。
  - **新增测试把 `tests/exchange/main.rs` 推过 500 行**：按场景拆成 `main.rs`（注册表/构造拒绝/顺序/Debug/错误映射）+ `response.rs`（respond 的四种结果）+ `stream.rs`（分片与流失败），没有删断言或压缩。

  门（真实输出）：`cargo +nightly fmt --all --check` 通过；`cargo clippy --workspace --all-targets --all-features -- -D warnings` 无 issue；`cargo test --workspace --all-features` 757 passed / 0 failed（39 suites，`nico-llm` 73 个）；`cargo deny check` 四项全 ok；`RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` 通过；`cargo check --no-default-features --workspace` 通过；`cargo tree -p nico-http | grep nico-llm` 无输出；文件预算只剩既有的 `crates/nico-config/src/cache.rs`（554 行，本任务未触碰，属独立 backlog）。

  复核确认（非新增）的约束：`schema/` 仍只依赖 serde 与基础类型；厂商名仍只出现在 `protocol/<name>` 与 `provider/<name>`（`tests/structure.rs` 覆盖 `protocol/` 整目录，`facts.rs` 已纳入）；`CanonicalError::from_static_parts` 仍是 crate 内唯一不校验的构造路径，由 `every_failure_maps_to_a_valid_canonical_error` 兜底。

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
- [x] spec：`directory-structure.md` 的 `nico-llm` 模块树与“无 vendor switch”规则；`error-handling.md` 的 canonical error 表（第 2 步复核轮完成）。
- [ ] spec：`pingora-guidelines.md` 的 AI 缓冲/转换；`logging-guidelines.md` 的 `llm.*`（随第 5 步数据面缝）。
- [ ] `implement.jsonl`/`check.jsonl` 包含 design、research、相关 spec。
- [ ] **门 A**：`cargo +nightly fmt --all --check`、`cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`、`cargo test --workspace --all-features`、`cargo deny check`、`RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`。
- [ ] **门 B**：依赖边界（`nico-http` 无 `nico-llm`；`nico-core` 无网络/密码学）、文件预算、`(protocol, provider)` matrix harness 可运行、无 vendor switch 结构测试。

## 回滚点

- 第 1 步（schema）独立提交：只增类型/测试，可单独回退。
- 第 3/4 步（codec/provider）不触碰数据面，可单独回退。
- 第 5 步（数据面缝）是行为变更，单独提交；回退即恢复 provider 路由拒绝 + 无 AI 缓冲。
- 每一步都在 `implement.md` 勾选并记录门禁输出；红的门不绕过（`SKIP_NICO_HOOKS` 仅工具故障）。
