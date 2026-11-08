# 1.4b 实施计划：先发布期事实，再按轴补适配器

> 设计见 `design.md`。顺序是硬约束：三处发布期事实（§2）与共享 wire 助手落地之前不写任何新适配器；每个 provider 与每个 codec 各自独立，可以用 wire fixture 单独验收，最后的矩阵步骤才需要它们全部在位。

## 0. 规模与并行

- 新增约 30 个源文件、约 6k 行（含测试），步骤 1–9。
- 依赖链：`1 → 2 → {3, 4, 5} → {6, 7, 8} → 9`。三个 provider（步骤 3/4/5）与三个 codec（步骤 6/7/8）各自只碰自己的目录，可以在互不重叠的文件集上并行；步骤 9 必须在全部落地后串行执行。
- 每一步结束都要保持工作区绿：`cargo +nightly fmt --all -- --check`、`cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`、`cargo test --workspace --all-features --locked`。

## 1. 发布期事实（契约扩展 + 配置）

- [ ] `provider/endpoint.rs`：`Endpoint::with_model(ModelId)` / `Endpoint::model() -> Option<&ModelId>`（`Debug` 打印模型）；新增 `EndpointError::RouteFact { field: &'static str, reason: Cow<'static, str> }`，文档写明它是**发布期**失败（保留 LKG）。
- [ ] `provider/route.rs`：`ProviderRoute::with_max_output_tokens(NonZeroU32)` / `max_output_tokens() -> Option<NonZeroU32>`；`Debug` 带上它。
- [ ] `filter.rs::Exchange::prepare`：解析模型、过允许列表之后，`endpoint.model()` 存在且不等于物理名 → `ExchangeError::ModelNotAllowed`（403）；`prepare`/`respond` 的调用顺序在测试里断言（endpoint 仍是编译期单次）。
- [ ] `provider.rs`：`ProviderError::InvalidEndpoint` 与 `ExchangeBuildError::Endpoint` 的文档扩到“缺少本适配器要求的路由事实也算”；`EndpointError::RouteFact` 通过既有的 `EndpointError` re-export 暴露。
- [ ] `nico-core`：`providers[].max_output_tokens: Option<u32>`（`raw`/`config/provider.rs`/`semantic` 校验 >0/`reference` 不需要改/`document` 测试）；shipped 文档加一行注释说明语义。
- [ ] `nico-server/llm/plan.rs`：把 `entry.max_output_tokens()` 编进 `ProviderRoute`；plan 测试断言编译后的值。
- [ ] **检查点 A**：`cargo test -p nico-llm -p nico-core -p nico-server`；既有 OpenAI 路径全绿（`Endpoint::model()` 为 `None` 时行为不变）；`prepare` 的顺序与 403 有测试；文件 ≤500 行。

  **验收要点**
  - 钉模型不通过时不发生任何 encode（测试用 mock provider 断言调用序列）。
  - `max_output_tokens` 的拒绝是文档错误（`nico-core`），不是发布期错误；它只在适配器要求时由 `RouteFact` 在发布期拒绝。

  **落地记录（第 1 步）**
  - 新增/修改：`provider/endpoint.rs`（`Endpoint::with_model`/`model` + `EndpointError::RouteFact`，`Debug` 打印模型）、`provider/route.rs`（`NonZeroU32` 的输出上限缺省）、`filter.rs::prepare`（允许列表之后、encode 之前比对钉模型 → 403 `model_not_allowed`）、`provider.rs`/`filter/error.rs` 的文档扩写；`nico-core` 的 `providers[].max_output_tokens`（raw/provider/semantic/validate + `document/tests/providers.rs`）；`nico-server/llm/plan.rs` 抽出 `route_facts()` 并把上限编进路由事实（零值不 panic，转成 `RouteError`）。
  - 测试：`tests/exchange/models.rs` 三个新用例（匹配 → 正常 encode、不匹配 → 403 **且 trace 停在 decode**、无钉模型 → 行为不变），`tests/contracts.rs` 覆盖 `Endpoint::model`/`RouteFact`/`max_output_tokens` 的形状与 `Debug`；`nico-server` 的 `tests/plan/facts.rs` 断言“文档值 → 路由事实”这条接线（上限 + 允许列表），因为单元测试测不到接线。
  - 一次体积修正：新增测试把 `tests/plan.rs`（531）与 `document/tests.rs`（517）推过 500 行硬上限，按数据边界拆出 `tests/plan/facts.rs`（91）与 `document/tests/providers.rs`（64），不是压缩断言。
  - 门（真实输出）：`cargo test --workspace --all-features --locked` **1142 passed / 47 suites**；`cargo +nightly fmt --all -- --check` 通过；`cargo clippy --workspace --all-targets --all-features --locked -D warnings` 无 issue；`RUSTDOCFLAGS=-D warnings cargo doc` 通过；全树无文件 >500 行。

## 2. 共享 wire 助手

- [ ] `src/wire.rs` + `src/wire/base64.rs`：RFC 4648 编解码（编码从 `provider/openai/base64.rs` 迁移，解码新增；padding 缺失/非法字符/长度错误都是 typed 错误），向量测试含高位字节、空输入、缺失 padding。
- [ ] `src/wire/json.rs`：`parse`（只报类别与行列）、`error_class`、`reject_extra`（未知成员，引用规则走 `diagnostic`），从 `provider/openai.rs` 抽出来，`provider/openai` 改为引用。
- [ ] `protocol/openai_chat/message.rs`：`image_url` 的 `data:` URL 不再拒绝，改为 base64 解码成 `ImageSource::Base64`（1.4a 拒绝的理由是“没有解码器”，现在有了；矩阵 fixture 需要一种四协议都能写出的图片拼写）；坏 base64/非 image media type 的拒绝有测试。
- [ ] `tests/structure.rs`：扫描列表加 `wire`（vendor-free）。
- [ ] **检查点 B**：既有 1.4a 测试全绿（只在 `data:` 图片这条上预期从“拒绝”改为“解码”，对应测试同步重写）；新助手只有单测消费者时允许 `#![expect(dead_code, reason = "…")]`，直到步骤 3 用到并删除。

  **落地记录（第 2 步）**
  - 新增 `src/wire.rs`（17）、`src/wire/base64.rs`（223）、`src/wire/json.rs`（180）；删除 `provider/openai/base64.rs`（编码迁入 `wire/base64.rs`，解码新增：非 4 的倍数 → `Length`、字母表外字符 → `Character`、`=` 不在末尾或超过两个 → `Padding`，三者 `Display` 都不引用 payload）。
  - `provider/openai.rs` 的 `parse`/`reject_extra` 变成对 `wire::json` 的两行映射（错误文本逐字不变），`error_class` 由 `provider/openai/request.rs` 直接引用；`protocol/openai_chat/message.rs` 的 `image_url` 接受 `data:<image/*>;base64,<payload>`，其余 data URL 形式按字段拒绝（非 base64 → `UnsupportedField`；无 `,`/空 media type/非 `image/*`/带参数/空 payload/坏 base64 → `InvalidRequest`），拒绝句只报字段路径。`tests/structure.rs` 的 `VENDOR_FREE` 加 `wire.rs` + `wire`。
  - `wire` 的每一项本步即有生产消费者（`encode` 由 OpenAI adapter、`decode` 由 chat codec、三个 JSON 助手由 adapter），因此没有加 `#![expect(dead_code)]`。
  - 测试：`wire/base64.rs` 8 个（RFC 4648 双向向量、全字节值、1..512 字节往返、缺失 padding/非法字符/错位 padding、拒绝不引用 payload），`wire/json.rs` 6 个（类别+行列、空文档、形状不符 = `data`、未知成员引用与不引用），`tests/openai_chat/message.rs` 的 data URL 断言由“拒绝”改为“解码 + 6 条拒绝用例”。
  - 门（真实输出）：`cargo test --workspace --all-features --locked` **1156 passed / 47 suites**；`cargo +nightly fmt --all -- --check` 通过；`cargo clippy --workspace --all-targets --all-features --locked -D warnings` 无 issue；`RUSTDOCFLAGS=-D warnings cargo doc -p nico-llm --no-deps` 通过；全树无文件 >500 行。

## 3. Anthropic provider adapter

- [ ] `provider/base_url.rs`：把 `provider/openai/url.rs` 里与厂商无关的部分（scheme/authority/port/host 规则、前缀与空片段拒绝、`join(operation_path)`）抽成共享解析，`provider/openai/url.rs` 变为薄封装（chat 保留既有行为，新增 `/responses` 后缀）；既有 base URL 测试全部保留。
- [ ] `provider/anthropic.rs` + `anthropic/{url,request,response,stream,error,usage}.rs`：模块文档写字段表（canonical ↔ Messages），`PROVIDER_KIND = "anthropic"`、`NATIVE_PROTOCOL = "messages"`、`version() = "2023-06-01"`。
- [ ] `endpoint`：`base_url::parse` + `/v1/messages`；`x-api-key` + `anthropic-version`；发布期缺 `max_output_tokens` → `RouteFact`。
- [ ] `encode` / `decode_response` / `stream_decoder` / `error_for_status` 按 `design.md` §4.2 与 §5.1 的表；`capabilities()` 不声明 `reasoning`，不声明 `response_format`。
- [ ] `builtin_registry()` 注册；`tests/registry.rs` 与 `tests/matrix.rs` 的既有行不变。
- [ ] 测试：`tests/anthropic_provider/{main,endpoint,request,response,stream,exchange,support}.rs`，覆盖 system 顶层字段、tool_use/tool_result 折叠与分组、thinking block、`stop_reason` 全表、usage 四个字段、`max_tokens` 缺省、任意分片流、缺 `stop_reason` 的截断、`ping`/未知事件、错误状态表、Debug 脱敏。
- [ ] **检查点 C**：单 crate 全绿；`exchange` 测试断言 `anthropic.messages` codec 缺席时只用 mock codec 也能配对（证明两侧独立）。

  **落地记录（第 3 步）**
  - 新增 `provider/base_url.rs`（247，vendor-free 的 base_url → destination + 前缀 + 操作 path，`openai/url.rs` 退化为 61 行薄封装并留出操作后缀参数给第 4 步）、`provider/anthropic.rs`（265）与 `anthropic/{url,request,request/conversation,response,stream,stream/decoder,error,usage}.rs`（71/256/288/247/150/389/178/61）；`provider.rs`、`lib.rs` 注册与模块文档；`tests/structure.rs` 把 `provider/base_url.rs` 纳入 vendor-free 扫描。
  - `NATIVE_PROTOCOL = "anthropic.messages"`（与 codec id 同一拼写，passthrough 的相等校验才成立）；`version() = "2023-06-01"`；`capabilities()` 不含 `reasoning`/`response_format`/`files`。
  - 发布期：`endpoint()` 缺 `max_output_tokens` 直接 `RouteFact`，所以 `encode` 里那条同样的检查在生产里不可达，只是防止有人绕过发布期直接构造路由。
  - 请求期拒绝：`Item::Reasoning`、`seed`/两个 penalty、`temperature > 1.0`、`ResponseFormat != Text`、`ContentPart::File`、会话只含 system 或首条不是 user。`ToolResult::is_error` 是该协议**能**表达的字段，正常写出。
  - 流式：block index 由 decoder 分配；`message_delta` 可多次；`[DONE]` 不存在（`message_stop` 才是终止符）；未宣布/未闭合 block、缺 `stop_reason` 一律 `Malformed`。`signature_delta` 在流里丢弃（canonical 事件集没有放签名的位置），已记入 `design.md` §10。
  - 测试：`tests/anthropic_provider/` **112 个**；工作区 **1272 passed / 48 suites**；五条命令与文件预算全绿。

## 4. OpenAI Responses provider adapter

- [x] **研究检查点（先做）**：把 `provider/openai_responses` 将要写/读的每个字段对一遍厂商参考文档，结论记进 `.trellis/tasks/11-05-s1-4b-nico-llm-protocols/research/responses-wire.md`（字段、必填性、`function_call` 的 `id` 与 `call_id` 语义、reasoning item 的回放要求、`store` 的默认值）。实证不到的字段一律落成 typed 拒绝，并在模块文档的表里标注。
- [x] `provider/openai_responses.rs` + `openai_responses/{request,response,stream}.rs`：`PROVIDER_KIND = "openai_responses"`、`NATIVE_PROTOCOL = "responses"`、`version() = None`。
- [x] `provider/openai/{error,url}` 改为 `pub(super)`；`url::endpoint` 增加 path 后缀参数（chat 传 `/chat/completions`，responses 传 `/responses`），既有 chat 测试不动（`base64` 已在步骤 2 迁到 `wire/`）。
- [x] `encode` 恒写 `store: false`；§4.3 的拒绝表全部有测试；`decode_response` 对未知 output item 类型 → `Malformed`；`stream_decoder` 的 `output_index` → canonical `index`。
- [x] 测试：`tests/openai_responses_provider/…`，覆盖 `input` 字符串与 item 数组、instructions 合并、`function_call_output`、`text.format` 三种、`status`/`incomplete_details`、`response.completed` 缺 usage、`response.failed`、未知事件跳过、任意分片。
- [x] **检查点 D**：`research/responses-wire.md` 存在且字段表与代码一致；chat 方言的既有测试零改动通过。

  **落地记录（第 4 步）**
  - **研究检查点先做**：`research/responses-wire.md` 是按字段的三档表（`verified` / `unverified` / `refused`）。本机无网络，因此没有可引用的厂商文档：`unverified` 的四个决定是「不凭记忆写进 wire」——`text.verbosity` 拒绝、`function_call_output.output` 的数组形态拒绝、响应对象的请求回显集合按显式列表消费（`response::ENVELOPE`）、assistant 的 `output_text` 输入拼写按同一厂商 `output[]` 的用法写入并在模块文档标注。每个 `refused` 都写明重开条件。
  - 新增：`provider/openai_responses.rs`（295）+ `openai_responses/{request,response,stream,usage}.rs`（477/462/420/87）。`provider/openai/{error,url}` 从 `pub(super)` 放宽到 `pub(in crate::provider)`（模块与 `operation`/`failure`/`stream_failure` 三处），chat 方言的测试零改写通过；`provider.rs` 声明并 re-export，`lib.rs` 的 `builtin_registry()` 变成 3 个 kind 并在模块文档点名两个 OpenAI 方言，`tests/registry.rs` 断言 3 个 kind。
  - **与计划文字的两处差异（以 `design.md` 为准）**：`NATIVE_PROTOCOL` 是 `"openai.responses"` 而不是 `"responses"`（design §2.2 的 kind 表，且 passthrough 合法是「相等」，必须与客户端 codec 的 protocol 同拼写；第 3 步的 `anthropic.messages` 同理）；列表里的「`input` 字符串」是客户端 codec 的解码方向，provider 适配器只编码且恒写 item 数组（一种拼写），由 `tests/openai_responses_provider/request.rs` 断言。
  - 拒绝路径：`Sampling.stop`/`seed`/两个 penalty → `UnsupportedField` 422；`Item::Reasoning` → `Unsupported{Reasoning}` 422；file part → `Unsupported{Files}`；assistant/instruction 轮次的图片、failed tool result、tool result 里的图片 → 名字段 422；`previous_response_id`/`reasoning`/`truncation`/`parallel_tool_calls`/`include`/`service_tier`/`background`/`prompt` 从中继的 `extensions` 键 → 名字段 422；`store`/`text` 等本适配器自己写的成员 → shadowing `400`。
  - 响应：`status` + `incomplete_details.reason` 的闭表（`completed`+call → `ToolCalls`，`incomplete` 两因，`failed` → `Error`，`in_progress`/`queued`/`cancelled`/未知 → `Malformed`），未知 output item / 未知成员 / `refusal` 部件 / 非空 citations → `Malformed`，reasoning summary 仍渲染为 `AssistantTurn::reasoning`（signature 为空），`function_call` 的 `fc_…` id 与 reasoning 的两个句柄都不进 canonical。
  - 流式：block/call 由 `output_index` 记，`ToolCallStart` 在第一个参数分片（或 `.done`）写出，`.done` 的累积串只在没有分片时写一次，`response.completed` 先关闭未闭合 call 再 `Usage`+`Finish`（由「本答复是否调用过工具」决定 `ToolCalls`/`Stop`，`self.called` 而不是尚未清空的 map）；`response.completed` 之后字节忽略；`[DONE]` 忽略但**不是**终止符；缺 `response.completed` → `Malformed`。
  - 测试：`tests/openai_responses_provider/` **105 个**（`main`/`endpoint`/`request`(+`request/items`)/`response`(+`response/items`)/`stream`(+`stream/calls`)/`exchange`/`support`）。三次体积修正都是按数据边界拆文件（`request.rs` 501、`response.rs` 507、`stream.rs` 533 → 拆出三个子模块），不是压缩断言。
  - 门（真实输出）：`cargo test --workspace --all-features --locked` **1377 passed / 49 suites**；`cargo +nightly fmt --all -- --check` 通过；`cargo clippy --workspace --all-targets --all-features --locked -D warnings` 无 issue；`RUSTDOCFLAGS=-D warnings cargo doc -p nico-llm --no-deps` 通过；全树无文件 >500 行。

## 5. Gemini provider adapter

- [x] `provider/gemini.rs` + `gemini/{url,request,parts,response,stream,fold,error,usage}.rs`：`PROVIDER_KIND = "gemini"`、`NATIVE_PROTOCOL = "gemini.generate_content"`、`version() = "v1beta"`。
- [x] `endpoint`：path = `base_url::parse` 的前缀 + `/v1beta/models/{physical}:streamGenerateContent?alt=sse`（query 由 `Endpoint::path` 携带，`set_raw_path` 接受 `path?query` 已实证）、`x-goog-api-key`、`Endpoint::with_model(physical)`；非常量 `model_map` → `RouteFact`；`mode: passthrough` → `RouteFact`。
- [x] `encode` / `parts` 按 §4.4 的表反向（`inlineData`、`functionCall/functionResponse`、`thought`/`thoughtSignature`、`responseSchema`、`toolConfig`）；图片 URL 与 `is_error` 的拒绝有测试。
- [x] `stream_decoder` + `fold`：`decode_response` 把 SSE body 折叠成 `ChatResponse`（复用同一事件源），`finishReason` 全表，`responseId` 缺失时用常量并写明理由。
- [x] 测试：`tests/gemini_provider/…`，覆盖 path 含钉住模型与 `alt=sse`、发布期拒绝（非常量 map / passthrough）、模型不匹配 403（经 `Exchange`）、`functionCall` 缺 id 时按位置分配、`functionResponse` 对象 ↔ 文本、`thought` 往返、`fold` 的文本/工具/usage/finish 组合、任意分片。
- [x] **检查点 E**：`(openai.chat → gemini)` 的 exchange 级测试通过：流式与缓冲两条路都断言同一 canonical 答复。

  **落地记录（第 5 步）**
  - 新增 `provider/gemini.rs`（283）与 `gemini/{url,request,parts,response,stream,fold,error,usage}.rs`（127/492/297/177/265/348/157/62）；`provider.rs` 声明并 re-export，`lib.rs` 的 `builtin_registry()` 变成 4 个 kind，`tests/registry.rs` 断言 4 个 kind。`NATIVE_PROTOCOL` 用 `gemini.generate_content`（design §2.2 的 kind 表；passthrough 合法是「相等」，必须与客户端 codec 的 protocol 同拼写），计划文字里的 `generate_content` 按 design 修正。
  - **发布期**：`url::pinned_model` 是唯一的模型来源——`model_map` 必须非空且所有值相同（两个别名指向同一模型合法），并且该名字必须能作为**字面 path 段**（仅字母、数字、`.`、`_`、`-`），否则 `RouteFact { field: "providers[].model_map" }`：不转义，因为转义会让部署指向操作者没写的资源，未转义的 `?`/`#`/`/` 会让模型名改写请求行。`mode: passthrough` → `RouteFact { field: "routes[].mode" }`，在地址解析之前判定。端点带 `Endpoint::with_model(physical)`，请求期由 `filter.rs::prepare` 比对 → 403 `model_not_allowed`（`tests/gemini_provider/exchange.rs` 断言该 adapter 上真的生效）。
  - **恒流式 + fold**：`response::decode` 把整段 body 喂给 `stream::Decoder`（与 `Provider::stream_decoder` 同一个事件源），再把事件交给 `fold::build`。`fold` 是 crate 私有、只被 `decode_response` 使用；事件级拒绝（重复 `Start`、未知 index、半开 call、缺 `Finish`、`Error` → `Upstream`）无法从公开路径构造，因此是 `fold.rs` 内的单测，`tests/gemini_provider/fold.rs` 覆盖公开路径能观察到的组合。
  - **两处与 design 文字的差异（以行为正确为准，已写进模块文档）**：① `functionCall` 解码写 `ToolCallStart` + **一个携带整段 arguments 文本的 `ToolCallDelta`** + `ToolCallEnd`——design §5.3 只写了 Start/End，但丢掉 delta 就丢掉参数，而 §4.4 的客户端编码器正是「累积到 `ToolCallEnd` 才写出完整 `args`」；② 响应里 `promptFeedback.blockReason`（design 未提）折叠成 `Finish(ContentFilter)` 的无内容答复，否则被过滤的 prompt 会变成「缺 finish reason」的 502。
  - **另外三个新增的显式判定**：`modelVersion` 缺失 → `Malformed`（canonical 答复必须命名物理模型，design 只写了 `responseId` 的常量兜底）；`thoughtSignature` 在解码方向读出后丢弃（canonical 事件集只有 `ReasoningDelta(text)`，与 Anthropic 的 `signature_delta` 同类缺口，模块文档写明）；扩展键 `model` 也按 shadowing 拒绝（该协议允许 body 里出现 model，写进去会与 path 钉住的模型冲突）。
  - 测试：`tests/gemini_provider/` **118 个**（`main`/`endpoint`/`request`(+`request/conversation`)/`parts`/`response`/`stream`/`fold`/`exchange`/`support`）+ `fold.rs` 内 **11 个**事件级单测。`tests/gemini_provider/request.rs` 一次体积修正：532 行超过硬上限，按边界拆出 `request/conversation.rs`（254），不是压缩断言。`nico-server/src/llm/tests/runtime.rs` 的「五轴能力行数」断言从 2 改成 3 并补 Gemini 的 `llm.version=v1beta`（新 adapter 是第三个声明满五轴的侧）。
  - 门（真实输出）：`cargo test --workspace --all-features --locked` **1506 passed / 50 suites**；`cargo +nightly fmt --all -- --check` 通过；`cargo clippy --workspace --all-targets --all-features --locked -D warnings` 无 issue；`RUSTDOCFLAGS=-D warnings cargo doc -p nico-llm --no-deps` 通过；全树无文件 >500 行。

## 6. Anthropic 客户端 codec

- [x] `protocol/openai_error.rs`：从 `protocol/openai_chat/error.rs` 抽出 OpenAI 的客户端错误包络（第二个消费者是 responses codec），chat 侧改引用，测试不动。
- [x] `protocol/anthropic.rs` + `anthropic/{request,blocks,response,stream,error,usage}.rs`：`PROTOCOL = "anthropic.messages"`，`version() = "2023-06-01"`。
- [x] decode：system 顶层、content blocks（text/image/tool_use/tool_result/thinking）、tools/tool_choice、`max_tokens` 必填、`temperature > 1.0` 拒绝、`thinking` 开关拒绝、无副作用标签 → `extensions`。
- [x] encode：响应文档、`stop_reason` 反向表（`ContentFilter` → `refusal`）、usage、错误包络（类别 → `error.type` 闭表）。
- [x] `stream_encoder`：per-response 的 block index 记账、`message_delta` 可多次、`message_stop` 在 `finish()`、`Error` 之后不再写。
- [x] 测试：`tests/anthropic_client/{main,request,blocks,response,stream,error,support}.rs`。
- [x] **检查点 F**：任意 1..N 字节分片的 wire fixture 解码结果一致；`(anthropic.messages → anthropic)` 的转义往返（wire A → canonical → wire A）语义相等。

  **落地记录（第 6 步）**
  - 抽取 `protocol/openai_error.rs`：两个 OpenAI 客户端协议共用的 `{"error":{message,type,param,code}}` 包络（`document`/`frame`），`openai_chat/error.rs` 退化成两行委托，既有测试零改动。
  - 新增 `protocol/anthropic.rs`（`AnthropicMessagesCodec`、`PROTOCOL`、`VERSION = "2023-06-01"`、`finish_reason` 反向表）与 `anthropic/{request,blocks,response,stream,error,usage}.rs`。
  - decode：顶层 `system`（字符串或 text 块数组）→ 前导 system item；消息内容按块顺序折叠（text/image 成一段 message，`tool_use`/`tool_result`/`thinking` 各自成 item，flush 逻辑因此保留 wire 顺序）；`tools`/`tool_choice`（`auto`/`any`/`tool`/`none`）；`max_tokens` 必填（缺失 400）、`temperature > 1.0` 400（不 clamp）；`thinking`/`top_k`/`service_tier`/`mcp_servers`/`container`/`context_management` 422；`tool_choice.disable_parallel_tool_use` 422；`metadata` 与未知顶层字段进 `extensions`；`redacted_thinking`/`document`/server-tool 块 422，未知块类型 400。
  - encode：单候选（多个候选是 `Encode` 错误，协议只有一条 assistant message）；块顺序 thinking → text → tool_use；`FinishReason` 反向表（`ContentFilter` → `refusal`，`Error` → `end_turn`）；`usage` 恒写 input/output（缺则 0，因为该协议的 usage 是必填对象），两个 cache 计数按报告值写；扩展键与本文档成员冲突 → `Encode`。
  - stream encoder：per-response 的 block index 记账（文本/thinking/每个 tool call 各占一块，类型切换先 `content_block_stop`）；`Finish` → `message_delta`，晚到的 `Usage` → 第二个 `message_delta`，`finish()` 写 `message_stop`；`Error` → `error` 事件且不写终止符；未开块就来的 fragment → `Encode`。
  - error：`{"type":"error","error":{"type":…,"message":…}}`，类别→type 闭表，**不写 code**（该协议没有这个成员），provider code 不进响应。
  - `protocol.rs` 注册新 codec、`lib.rs` 的 `builtin_registry()` 变成 2 个 codec、`tests/registry.rs` 断言 2 个协议并断言 `version()`。
  - 测试：`tests/anthropic_client/{main,support,request,response,stream,error}.rs` **31 个**（system 两种拼写、块顺序、base64/URL 图片、tools/tool_choice、采样、缺 `max_tokens`、温度越界、五个已知字段拒绝、preserved 字段、未知块 vs 已知块、usage/扩展/停止原因、流式 6 个场景、错误包络三态）。工作区 **1537 passed / 51 suites**；fmt/clippy/doc/文件预算全绿。

## 7. OpenAI Responses 客户端 codec

- [x] `protocol/openai_responses.rs` + `openai_responses/{request,items,response,stream,error,usage}.rs`：`PROTOCOL = "openai.responses"`，`version() = None`。
- [x] decode：`input` 字符串/item 数组、`instructions`、`text.format`、`tools`（扁平 function 形状）、`tool_choice` 四种、拒绝表（§4.3）、`metadata`/`user` → `extensions`。
- [x] encode：`output[]`（message/output_text、function_call）、`status`/`incomplete_details`、usage。
- [x] `stream_encoder`：`sequence_number` 递增、`output_item.added`/`content_part.added`、累积出 `response.completed` 的完整 `output`、无 `[DONE]`。
- [x] 测试：`tests/openai_responses_client/{main,request,items,response,stream,error,support}.rs`，含 `previous_response_id`/`store: true`/内置工具的拒绝与“不静默丢”。
- [x] **检查点 G**：`(openai.responses → openai_responses)` 的往返语义相等；错误包络与 chat 方言共用一份实现（测试断言两者输出结构一致）。

  **落地记录（第 7 步）**
  - 新增 `protocol/openai_responses.rs`（`OpenAiResponsesCodec`、`PROTOCOL = "openai.responses"`、无版本拼写）与 `openai_responses/{request,items,response,stream,error,usage}.rs`；错误包络直接复用 `protocol/openai_error`（与 chat 方言同一份实现），`lib.rs` 的 `builtin_registry()` 变成 3 个 codec、`tests/registry.rs` 断言 3 个协议。
  - decode：`instructions` → 前导 system item；`input` 是字符串或 item 数组，每个 item 恰好一个 canonical item（`message`/`function_call`/`function_call_output`）；`input_image` 同时接受 http(s) 与 `data:`（base64 解码）；`tools` 是扁平 function 形状（先读 `type` 再类型化解码，所以内置工具报 `tools[i].type` 而不是“缺 name”）；`tool_choice` 三种模式 + 具名；`text.format` 三种；`store: false` 接受、`true` 422；12 个已知字段（`previous_response_id`/`reasoning`/`truncation`/`parallel_tool_calls`/`include`/`service_tier`/`background`/`prompt`/`stop`/`seed`/两个 penalty）422；`metadata`/`user` 进 `extensions`；`reasoning` item → 422 缺能力；会话中间的 system/developer 消息 → 422 按字段。
  - encode：`status` 闭表（`Stop`/`ToolCalls` → completed，`Length` → incomplete+`max_output_tokens`，`ContentFilter` → incomplete+`content_filter`，`Error` → failed）；`output[]` 是 message/function_call（reasoning summary 有则一并渲染）；`usage` 用该协议自己的 `input_tokens`/`output_tokens`/`total_tokens` + 两个 details。
  - stream：`sequence_number` 从 0 递增、事件名与 payload `type` 同拼写；文本（item.added → content_part.added → delta → done → item.done）与工具（item.added → arguments.delta → item.done）；**`response.completed` 推迟到 body 结束写出**（`finish()`），因为 provider 可能在 finish 之后才报 usage，而该协议只有这一个终止事件；没有 finish 事件就什么都不写（不把截断说成完整）；失败走共用包络的 `data:` 帧。
  - 修了两处既有测试的“未知协议”夹具：`openai.responses` 现在是本 build 服务的协议，改成 `openai.assistants`（`nico-server` 的 plan 与 publication 各一处）。
  - 测试：`tests/openai_responses_client/{main,support,request,response,stream,error}.rs` **25 个**。工作区 **1564 passed / 52 suites**；fmt/clippy/doc/文件预算全绿。

## 8. Gemini 客户端 codec

- [x] `protocol/gemini.rs` + `gemini/{target,request,parts,response,stream,error,usage}.rs`：`PROTOCOL = "gemini.generate_content"`，`version() = "v1beta"`。
- [x] `target`：从 `RequestFacts::path` 解析 `…/models/{model}:{operation}` 与 `alt`；未知操作/`alt` 非 `sse` 的拒绝；流式标志来自操作名。
- [x] decode/encode 按 §4.4 的表；`generationConfig` 全字段；`functionCall` 缺 id 时按位置补；URL 图片、`is_error`、`candidateCount > 1`、`safetySettings`、`thinkingConfig` 的拒绝。
- [x] `stream_encoder`：一帧一个 `GenerateContentResponse`、工具参数累积到 `ToolCallEnd`、`finish()` 时未闭合 → `Encode`。
- [x] 数据面缝隙：`LlmRequest` 的 path 传 `path_and_query`（`RequestFacts::path` 的文档已承诺含 query，数据面当前只传 `uri.path()`）；`nico-http` 侧一个断言，`nico-llm` 侧不加类型。
- [x] 测试：`tests/gemini_client/{main,target,request,parts,response,stream,error,support}.rs`。
- [x] **检查点 H**：`(gemini.generate_content → gemini)` 的 convert 往返；`alt`/操作名的拒绝表；非 UTF-8 path 不会 panic。

  **落地记录（第 8 步）**
  - 新增 `protocol/gemini.rs`（`GeminiGenerateContentCodec`、`PROTOCOL = "gemini.generate_content"`、`VERSION = "v1beta"`）与 `gemini/{target,request,parts,response,stream,error,usage}.rs`（192/258/408/464/160/275/75/46）；`lib.rs` 的 `builtin_registry()` 变成 4 个 codec、`tests/registry.rs` 断言 4 个协议并断言 `version()`。`CALL_ID_PREFIX` 的所有权移到 codec 模块（一个方言一条按位分配规则），provider 侧 re-export 同一常量，`provider::gemini::CALL_ID_PREFIX` 公开路径不变。
  - **target**：`…/models/{model}:{operation}` 与 query 从 `RequestFacts::path` 解析；operation 闭表（`countTokens` 等 → 400，可打印时点名）；`alt` 缺省或 `sse`，其他 → 422；**其余 query 参数一律按名拒绝**——canonical 没有承载 target 事实的位置，而该协议在此处的真实参数是 `key` 一类凭证，放进 preserved 桶会被 provider encoder 写回上游 body；escape 不解码（`%FF%FE` 等字面名流向下游的路由模型解析去拒绝），切片只在 ASCII 标记上取，任意字节序列（`/v1β/…`、多字节前缀）有类型化结果、不 panic。
  - **decode**：`systemInstruction` → 前导 system item（text-only，其余 422）；`contents[].role` 缺省按 user（协议自己的单轮缺省），`user`/`model` 之外 400；一轮的 text/inlineData 折叠进一条 message，`thought`(+`thoughtSignature`)/`functionCall`/`functionResponse` 各自成 item（wire 顺序 = item 顺序）；`functionCall.id` 缺失按位置补 `call_<n>`，`functionResponse` 缺 id 按同名 call 唯一匹配解析，无匹配或歧义 → 400（不猜配对）；`response` 必须是闭对象 `{"content": str}`（这正是 adapter 与 fixture 写的形状）；`fileData` → 422 `unsupported_capability(files)`；混类 part/空 part/坏 base64 → 400。`generationConfig` 全字段映射；`topK`/`thinkingConfig`/`candidateCount > 1` → 422，`candidateCount: 1` 接受（就是缺省），未知成员 → 400；`responseMimeType`：`application/json`(+schema)/`text/plain` 之外 422，schema 无 mime → 400。顶层：`safetySettings`/`cachedContent` → 422；body 里的 `model` 成员 → 400（path 已命名模型，第二个是钉住目标要防的静默切换）；`labels` 与未知字段 → extensions。
  - **encode**：单候选且 index 0（多个/偏移 → `Encode`）；parts 顺序 thought → text → call（模型产出顺序，fold 的逆）；`FinishReason` 反向表（`ToolCalls` 与 `Stop` 同写 `STOP`——该方言在调用后报的就是 STOP；`ContentFilter` → `SAFETY`；`Error` → `OTHER`）；`usageMetadata` 四桶按报告值写；答案里的 image/file part → `Encode`（四家 provider 都不产出这样的答复，写成员是虚构行为）；扩展照写、shadowing 拒绝。
  - **stream**：一事件一 chunk；`Start` → identity chunk（`responseId`+`modelVersion`，二次 Start → `Encode`）；文本/思考增量各自成 chunk；工具参数累积到 `ToolCallEnd` 写整 `args` 对象（不成 JSON/非对象 → `Encode`）；`Usage` 独立成帧，finish 之后仍写——该协议没有终止符帧，§7 的「最后一帧 usageMetadata」就是它；Finish 后内容忽略、二次 Finish 忽略；`finish()` 无终止符，仅未闭合 call → `Encode`（canonical 流总会闭合）。
  - **error**：`{"error":{code,message,status}}`，`code` = HTTP 状态数值（该包络自己对这个值的承载），`status` = 类别 → `google.rpc` token 闭表（Client→`INVALID_ARGUMENT`、Limit→`RESOURCE_EXHAUSTED`、Upstream→`UNAVAILABLE`、Cancelled→`CANCELLED`、Internal→`INTERNAL`），canonical 稳定 code 无字段不写；流中失败走共用 `data:` 帧，其后沉默。
  - **数据面缝隙**：`nico-http` 的 `request_body` 改传 `uri.path_and_query()`（非 origin-form 目标回退 `path()`），`proxy/tests/llm.rs` 加一条断言（`?alt=sse` 原样到达 front-end）；`nico-llm` 侧零类型改动。
  - 测试：`tests/gemini_client/{main,support,target,request,parts,response,stream,error}.rs` **46 个**，含检查点 H 的 `(gemini.generate_content → gemini)` convert 往返（codec decode 后经 provider adapter encode，与原文档逐值相等）、`alt`/操作名/未知 query 的拒绝表与非 UTF-8/转义 path 不 panic。`nico-server` 的「五轴能力行数」断言 3 → 4（gemini codec 是第四个声明满五轴的侧）。
  - 门（真实输出）：`cargo test --workspace --all-features --locked` **1618 passed / 53 suites**；`cargo +nightly fmt --all -- --check` 通过；`cargo clippy --workspace --all-targets --all-features --locked -D warnings` 无 issue；`RUSTDOCFLAGS=-D warnings cargo doc`（nico-llm 与 workspace）通过；全树无文件 >500 行。

## 9. 矩阵、端到端与 spec 收口

- [x] `tests/matrix.rs` → `tests/matrix/{main,rows,fixtures,support}.rs`（一个 target，沿用 `tests/exchange/main.rs` 的组织）：16 行矩阵 + 每行期望能力集合；passthrough 4 行（3 可发布、1 发布期拒绝且原因正确）。
- [x] `tests/fixtures/matrix/`：一份 canonical 对话 + 每个协议各一份 wire 文档；`fixtures.rs` 用 `support.rs` 的语义相等助手断言 decode/encode/adapter 双向相等（`extensions` 不参与比较）。
- [x] `nico-server/tests/llm_anthropic.rs`：Anthropic 客户端 → OpenAI 上游的端到端（含错误包络与流式）。
- [x] `nico-server/tests/llm_gemini.rs`：OpenAI 客户端 → Gemini 上游（上游 path 钉住模型 + `alt=sse`、缓冲答复从 SSE 折叠、模型不匹配 403、passthrough 发布失败保 LKG）。
- [x] `tests/structure.rs`：`wire` 纳入扫描；`PROVIDER_KIND`/`PROTOCOL` 常量拼写由测试锁定（日志字段是契约）。
- [x] spec：`directory-structure.md`（模块树 + `wire/`）、`logging-guidelines.md`（四个 kind 拼写 + `llm.version`）、`error-handling.md`（三张包络/状态表 + 422 字段拒绝）、`pingora-guidelines.md`（模型在 path 的三条约束）。
- [x] `config/resources.yaml`：注释里给出四个 protocol、四个 kind、`max_output_tokens` 的语义与 passthrough 的限制（不放需要凭证的活动路由）。
- [x] **检查点 I（收尾门）**：五条基线 + `cargo check --no-default-features --workspace` + `cargo tree -p nico-http --prefix none | grep nico-llm` 无输出 + 全树文件 ≤500 行（含新测试文件）。

  **落地记录（第 9 步）**
  - **矩阵**：`tests/matrix/{main,rows,fixtures,support}.rs`（170/128/243/267）。`rows.rs` 是显式 16 行表（4 codec × 4 adapter），每行携带设计 §6 的期望能力交集（`ALL`/`NO_REASONING`/`NO_FORMAT`/`NO_REASONING_NO_FORMAT` 四个 const 组合）+ 4 个原生对（`publishable: false` 标注 gemini 行）；`main.rs` 断言每行两侧都在注册表、能配对成 `Exchange`、`capabilities()` 逐位相等，且表长恰为 16（新半边不加行即失败）；passthrough 测试断言 3 行可发布（`PassthroughRoute` 的 protocol 是路线的）而 gemini 行拒绝原因为 `ProviderError::InvalidEndpoint → EndpointError::RouteFact { field: "routes[].mode" }`——是路由事实，不是 endpoint 解析失败。
  - **fixtures**：`tests/fixtures/matrix/` 16 份文档——每方言 4 份（客户端请求、客户端答复、provider 请求、provider 答复；gemini 的 provider 答复是 `.sse` 文本），全部是 `support.rs` 里同一份 canonical 请求（system、文本+内联图、assistant 文本、tool call+result、tools/tool_choice/max_tokens）与同一份 canonical 答复（text+reasoning、Stop、usage 11/7/3）的翻译。`support.rs` 的语义相等助手逐字段比较请求（model/stream/sampling/tool_choice/tools/conversation 逐 item）与答复（id/model/choices/usage），**`extensions` 不参与比较**；`fixtures.rs` 四个方向各断言一次（codec decode、codec encode、adapter encode、adapter decode），比较是解析后的 JSON 对 JSON。reasoning 不带 signature 是刻意决定：四家 provider 对 signature 的承载不一致（OpenAI 没有字段、Gemini fold 丢弃），带 signature 会让同一份答复无法在 16 行上成立。`#[ignore]` 的 dump 测试按需重印全部文档——fixture 是期望，不是代码上次的输出。
  - **结构**：`tests/structure.rs` 增加 registry 拼写锁定（四个 `PROTOCOL`、四个 `PROVIDER_KIND` 逐一对字面量，且四个方言对 `NATIVE_PROTOCOL == PROTOCOL`——passthrough 合法性依赖它）；`wire` 扫描第 2 步已在。
  - **端到端**：`nico-server/tests/llm_anthropic.rs`（Anthropic 客户端 → OpenAI 上游：完整答复、SSE 流（`message_start`→`message_stop`）、429 走 Anthropic 包络、`llm.protocol=anthropic.messages` 记录、`x-api-key` 不travel）；`nico-server/tests/llm_gemini.rs`（OpenAI 客户端 → Gemini 上游：请求行 `POST /v1beta/models/{physical}:streamGenerateContent?alt=sse`、`x-goog-api-key`、body 无 model 成员、缓冲答复从两帧 SSE 折叠、流式逐 chunk、模型不匹配 403 且上游零 body 字节、429 走 OpenAI 包络、`mode: passthrough` 的第二份文档发布被拒后旧代继续 200 并仍按 convert 翻译）。`support/http/resources.rs` 增加 `provider_of_kind`（现有 builder 硬编码 openai kind）。
  - **顺带修的真缝隙**：anthropic e2e 抓到转换路由只剥调用方的 `authorization`，`x-api-key`/`x-goog-api-key` 会原样到上游——调用方凭证描述的是调用方↔网关关系，转换请求把这段关系换成了路由自己的，所以三种拼写在 `nico-http` 的 `apply_upstream_request` 里一并剥除（`nico-http` 的 hook 测试与两条 e2e 各有断言）。
  - **spec**：`directory-structure.md` 的 nico-llm 模块树重写为当前形状（`wire/`、`protocol/openai_error`、三家新方言目录、`provider/base_url`、`fold`）并补两条 e2e；`logging-guidelines.md` 的 `llm.protocol`/`llm.provider` 行给全四种拼写并写明「第二个 OpenAI 方言是第二个 kind」、`llm.version` 列出本 build 的两个修订值；`error-handling.md` 增「四客户端错误包络」闭表（类别 → 各协议 type/token）并在 422 字段拒绝的例子里补齐四家方言；`pingora-guidelines.md` 的凭证规则改为三种拼写，并新增「模型在 path ⇒ 发布期钉住 + 恒流式 + passthrough 拒绝」三条约束。
  - **config**：`config/resources.yaml` 注释给出四个 kind、四个 protocol、passthrough 的 gemini 限制与 model_map 常量要求；不放需要凭证的活动路由。
  - 门（真实输出）：`cargo test --workspace --all-features --locked` **1626 passed / 55 suites**；`cargo +nightly fmt --all -- --check` 通过；`cargo clippy --workspace --all-targets --all-features --locked -D warnings` 无 issue；`RUSTDOCFLAGS=-D warnings cargo doc --workspace --no-deps` 通过；`cargo check --no-default-features --workspace` 通过；`cargo tree -p nico-http --prefix none | grep nico-llm` 无输出；全树无文件 >500 行。

## 复审重点（每一步之后）

1. **发布期 vs 请求期**：任何“每个请求都算一次”的事实（钉住的模型、输出上限、端点 path、能力交集）都必须在发布期算完。
2. **不静默丢**：新字段的三分表（modelled/preserved/refused）在模块文档里逐条能对上代码；拒绝只报字段名，不报值。
3. **归属**：成功状态归 codec（`ClientReply`），失败状态归 `CanonicalError`；`filter.rs` 不出现厂商名；`nico-http` 不 import `nico-llm`。
4. **能力声明 = 实际映射**：任何为了“看起来支持”而多加的能力位，都会被矩阵测试当作契约锁死。
5. **文件预算**：新目录按数据边界拆，出现的第 501 行是拆分信号，不是压缩信号。
