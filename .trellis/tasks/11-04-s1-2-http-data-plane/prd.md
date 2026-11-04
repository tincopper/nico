# gateway-http：Pingora 数据面骨架

## Goal

建立 `gateway-http`：把已发布的 `RuntimeSnapshot` 变成真实 HTTP 数据面 —— Pingora 接入、路由匹配、upstream 选择、流式字节透传、SSE、显式 `h2c`、显式 graceful 宽限期、正确的 1xx 与取消语义。**路由选择不读取请求 body。**

## Dependencies

- 前置：`1.1 route-schema`（需要 `Document.routes` / `upstreams` / `providers` 模型）。
- 后续：`1.3 gateway-plugins`（在本 crate 的 `HttpFilter` 上扩展）、`1.4 gateway-ai-passthrough`（依赖本 crate）。
- 不满足前置时不得开工。

## Requirements

### R1：crate 与依赖边界

- 新建 `crates/gateway-http`，依赖 `gateway-core` + `pingora`（`=0.9.1`）。
- 不 import `gateway-ai`、`gateway-plugins`；AI/策略通过扩展点接入。
- `gateway-server` 增加 `gateway-http` 依赖并负责组装；`gateway-core` 依赖树不变。

### R2：请求路径

1. 读取 `SnapshotHandle::current()`；无快照或未 ready → `503`（带稳定错误码）。
2. 按 `path_prefix` 最长匹配（同前缀按 `host` 精确匹配优先），无匹配 → `404`。
3. 选择上游：`upstream` 用 `scheme/host/port`；`provider` 留给 `gateway-ai` 的 Filter 处理（本任务只需保证路由元数据可用）。
4. 上游连接、请求头转发、body 流式透传（不整体缓冲、跨块状态由 Pingora 持有）。
5. 响应头与响应体透传；不做响应压缩；不注入 `server` 头（除非显式配置）。

### R3：阶段 0 实证约束

- `h2c = true` 必须显式配置；H1/H2 同端口服务。
- 上游为 gRPC 路由时显式 `ALPN::H2`；其它默认 H1。
- `upstream_response_filter` 遇到 1xx 不得插入响应头（按 `is_informational()` 分支）。
- 下游断开映射为 `Cancelled`（不记 500、不重试）。
- graceful：显式设置 `grace_period_seconds` 与 `graceful_shutdown_timeout_seconds`，并纳入 bootstrap 配置；默认值必须写清，不允许隐式 300 s。

### R4：资源上限与错误

- 请求体上限取 `route.max_body_bytes` 或文档 `limits.max_request_body_bytes`（取较小者），超限 `413`。
- 超时取 `route.timeout_ms` 或文档 `limits.request_timeout_ms`。
- 错误映射（按 `.trellis/spec/backend/error-handling.md`）：`Client`→400/401/403、`Limit`→413/429、`Upstream`→502/504、`Internal`→500、`Cancelled`→499（仅日志）。
- 错误响应体使用稳定 `code` 字段，不回显上游细节或凭证。

### R5：观测

- `tracing` 请求日志字段：`route_id`、`upstream`、`config.generation`、`http.status`、`elapsed_ms`；失败时加 `error.category`。
- 不引入 metrics exporter（阶段 1 之外）。

## Acceptance Criteria

- [ ] `curl --http1.1` 与 `curl --http2-prior-knowledge` 均能命中同一监听；未开 `h2c` 时 H2 明文失败（证明配置生效）。
- [ ] 路由匹配：最长前缀、host 精确优先、无匹配 404、无快照 503，各有测试。
- [ ] **不读 body 的路由**：mock 上游在收到首个 body 块之前已建立连接并收到请求头（用时间戳断言证明）。
- [ ] body 完整性：8 MiB 请求与响应经网关后 sha256 与对端一致；任意分块（1/3/随机字节）不丢字节、不改顺序。
- [ ] SSE 透传：mock 每 200 ms 一个事件，客户端按节奏收到；事件内容与 mock 发出的字节完全一致；不开启压缩。
- [ ] 1xx：`Expect: 100-continue` 场景下不在 100 响应上插入头（测试断言上游看到的 1xx 无附加头）。
- [ ] 取消：客户端中途断开后上游连接被关闭，日志归为 `Cancelled`，无 5xx。
- [ ] graceful：SSE 进行中触发停机，宽限期配置生效；测试断言配置值被读取且行为与配置一致（不依赖 300 s 默认）。
- [ ] 错误映射表逐项有测试（400/413/429/502/504/500/499 日志）。
- [ ] 依赖边界：`cargo tree -p gateway-core` 无 `pingora`；`cargo tree -p gateway-http | grep gateway-ai` 无输出。
- [ ] 基线五条命令全绿；新增测试使用手写 tokio TCP mock 上游（不引入 axum/hyper 作为测试依赖）。

## Out of Scope

- 过滤器链与策略执行（`1.3 gateway-plugins`）。
- provider 语义、凭证注入、SSE 转换（`gateway-ai`）。
- 重试与 fallback（阶段 1 之后，需先定重放语义）。
- TLS 终止与证书热更新、连接池调参、metrics exporter、管理 API。
