# gateway-ai：OpenAI Provider 与 SSE 透传

## Goal

建立 `gateway-ai`：AI 领域的 Filter 与 Provider 模块。阶段 1 只做 **OpenAI 兼容透传** —— 按路由级 `provider` 选择上游、注入凭证引用、透传请求与 SSE 响应，**不解析请求 body**，不做模型级路由，不做协议转换。

## Dependencies

- 前置：`http-data-plane`（Filter 扩展点、上游选择、响应流生命周期）。
- 前置：`route-schema`（`providers` 段与路由级 `provider` 引用）。
- 与 `gateway-plugins` 无顺序依赖，可并行。
- 依赖 `route-schema` 与 `http-data-plane` 均完成后才能开工。

## Requirements

### R1：crate 与依赖边界

- 新建 `crates/gateway-ai`，依赖 `gateway-core` + `gateway-http`；**不**被 `gateway-http` 反向依赖。
- `gateway-server` 增加依赖并注册 AI Filter。
- Provider 初期是模块（`provider::openai`），不拆微 crate。

### R2：Provider 配置与选择

- `provider.kind: openai`：字段 `base_url`、`credential_ref`、可选 `model_allowlist`、`timeout_ms`。
- 选择依据：路由已匹配到的 `provider` 名字（来自 `route-schema`），**不读取请求 body**。
- 凭证从 `credential_ref`（`env:VAR` / `file:/path`）解析，注入 `Authorization: Bearer ...`；文档不含明文。
- 凭证缺失或读取失败 → 启动/发布阶段失败（配置问题），不在请求路径上静默降级。

### R3：请求透传

- 路径与查询串原样转发到 `base_url`（不重写 `/v1/chat/completions` 语义，除非配置显式指定 `path_rewrite`）。
- 不解析、不重写请求 body；不假设 `model` / `stream` 在固定位置。
- 请求头：剔除 `Authorization`（由 provider 注入）、`Host` 由 upstream 决定；其它端到端头透传；不转发 hop-by-hop 头。
- 请求体上限沿用路由/文档限制；超限 413。

### R4：响应与 SSE

- SSE 透传：不转换事件、不聚合、不改写 `[DONE]`；不得开启响应压缩；不得整体缓冲。
- 任意字节分片、跨块 UTF-8、多行 `data:`、心跳注释都必须原样到达客户端（字节级一致）。
- 首字节提交后不得重试或换 provider；中途断流按 `Cancelled`/`Upstream` 记录，不向客户端补造事件。
- 上游 4xx/5xx 映射为 `Upstream` 错误；不得把 provider 错误体原样泄露给客户端（可透传稳定 code，不透传内部细节）。

### R5：观测

- 日志字段：`ai.provider`、`route_id`、`config.generation`、`http.status`、`elapsed_ms`、`stream=true/false`。
- 不记录 prompt、response body、`Authorization` 或 provider key。
- usage 统计与模型级指标留到阶段 2。

## Acceptance Criteria

- [ ] 路由级 `provider` 选中 OpenAI 上游；同一文档中普通 API 路由与 AI 路由并存，互不影响。
- [ ] 凭证注入正确（mock 断言 `Authorization` 存在且不含配置原文以外内容）；缺失凭证在配置阶段被拒。
- [ ] 请求 body 字节级透传：8 MiB / 任意分片的 sha256 与 mock 收到的一致。
- [ ] 响应字节级透传：普通 JSON 响应与 SSE 响应均一致；SSE 在 1/3/随机字节分片下客户端重组结果相同。
- [ ] SSE 时序：mock 每 200 ms 一个事件，客户端按节奏收到（不做整体缓冲）。
- [ ] 首字节后不重试：注入上游中途断流，断言没有第二次上游请求。
- [ ] 错误映射：上游 429/5xx → `Upstream`，客户端得到稳定错误码，不含 provider 细节。
- [ ] 日志不含 prompt/response/凭证（有测试断言）。
- [ ] 依赖边界：`cargo tree -p gateway-http | grep gateway-ai` 无输出；基线五条命令全绿。

## Out of Scope

- 按 `model` 路由、"有界缓冲 + 单次重放"（独立设计项）。
- Anthropic 等异构 Provider、请求/响应语义转换、usage 归一化、模型 fallback、熔断（阶段 2）。
- 多模态 payload 处理、token 计费、按 tenant 的 AI 配额。
- TLS/证书、metrics exporter、管理 API。
