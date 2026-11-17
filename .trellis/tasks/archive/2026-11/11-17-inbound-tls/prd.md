# 入站 TLS：rustls 终止与配置

## Goal

让 `nico` 的 `http.listen` 能直接终止 TLS，而不是必须在前面放一层代理。入站请求在网关完成 TLS 1.2/1.3 握手，再进入既有的 HTTP 数据面；`x-forwarded-proto` 对上游报告 `https`。

这是 `README` "当前限制" 中最后一项主要数据面能力：上游 TLS 已支持（`11-11-upstream-tls`），入站仍是明文。范围限定为**单证书、启动期加载**的 TLS 终止；证书热加载、mTLS、SNI 多证书、指标监听 TLS 不在本任务。

## Background

- `HttpSettings` 的 `tls` 目前只描述**上游信任根**；`service()` 固定 `add_tcp`。
- `upstream_request_filter` 目前对缺失的 `x-forwarded-proto` 固定写 `http`。
- `TlsSettings::build` 在证书/私钥不可读或不匹配时会 panic；启动期校验必须在本 crate 里完成，不能把坏文件留给 Pingora。
- 上游 TLS 的 fixtures 与 `TlsPolicy` 可复用，但入站证书/私钥是另一类事实：它们是 listener 的身份，不是信任根。

## Requirements

### R1：bootstrap 配置

- `http.tls` 是 `http` 段内的可选子段，字段 `cert_file`、`key_file`（路径）；两字段同时存在或同时缺失。
- 空的 `http.tls:`（无字段）是配置错误，不是“未启用”。
- 顶层 `tls.ca_file` 保持**上游信任根**语义，不变；`http.tls` 与它互不替代。
- `http.tls` 存在时，`http.h2c: true` 被拒绝：同一 listener 不能同时是明文与 TLS；错误字段 `http.h2c`。
- 未配置 `http.tls` 时行为与今天完全一致（明文 TCP，`h2c` 可选）。

### R2：启动期校验

- 读取并校验证书链与私钥：PEM 可解析、至少一个证书、私钥可用、证书与私钥匹配。
- 失败是启动失败（`BootstrapError::InvalidField` / service build error），不是第一个请求时的 500，也不是 panic。
- 错误只报字段与原因，**不打印证书、私钥或密钥材料**。
- 校验发生在 listener 绑定之前；失败时不进入 serving 状态。

### R3：TLS 策略

- rustls 后端（与上游 TLS 同一套依赖，不新增传输栈）。
- TLS 1.2 与 1.3；不启用 TLS 1.0/1.1。
- ALPN 通告 `h2` 与 `http/1.1`；HTTP/1.1 是 e2e 的请求路径，`h2` 的 ALPN 协商有测试。
- 单证书、无客户端证书（no client auth）；SNI 不改变证书选择（同一证书返回给所有名字）。
- 证书/私钥在启动时读取一次；文件变更需要重启，不做热加载。

### R4：数据面与转发

- 配置 TLS 时，`http.listen` 注册为 TLS listener；HTTP hooks、插件链、路由、LLM 交换、优雅停机均不变。
- `x-forwarded-proto`：入站 TLS 且客户端未提供时写 `https`；明文时写 `http`。不追加第二个值。
- 客户端对 TLS listener 发明文 HTTP：握手失败，不产生 HTTP 响应；不能静默降级到明文。
- `Host`、`x-forwarded-for`、`x-forwarded-host` 的现有规则不变。

### R5：错误与观测

- 缺失/不可读/无效证书或私钥：启动失败，错误字段指向 `http.tls.cert_file`/`http.tls.key_file`。
- service 构建失败（例如启动校验后文件消失）：`serve_with` 记录错误并返回失败退出码，不 panic。
- TLS 握手失败是连接级事实，不进入请求日志的 `http.status`；不伪装成 5xx。
- 成功请求的既有日志字段不变；`x-forwarded-proto` 变化可在上游 mock 中观察。

### R6：验证

- 单元：`HttpSettings` 的 TLS 配置、h2c 冲突、`InboundTls` 访问器；证书/私钥 PEM 校验（空、坏 PEM、不匹配）。
- bootstrap：`http.tls` 解析、空段、缺字段、坏文件、h2c 冲突。
- e2e：真实 TLS 客户端（`tokio-rustls`）完成握手并发出 HTTP/1.1 请求，上游 mock 收到 `x-forwarded-proto: https` 和正确路径；ALPN 客户端协商 `h2`；不受信根客户端握手失败；明文客户端不能得到 HTTP 响应。
- 现有明文 e2e 全部保持绿色；无 TLS 配置时不改变任何行为。

## Acceptance Criteria

- [x] `http.tls.cert_file`/`key_file` 可配置，启动期校验失败会拒绝启动且不泄露密钥材料。
- [x] 配置 TLS 后，真实 TLS 客户端可完成握手并访问既有 HTTP 路由；明文请求到该端口不能得到 HTTP 响应。
- [x] TLS 1.2/1.3、ALPN h2+h1 生效；ALPN `h2` 协商有测试。
- [x] 入站 TLS 请求到上游的 `x-forwarded-proto` 是 `https`；明文仍是 `http`。
- [x] `http.tls` 与 `h2c` 冲突、空段、缺字段、坏证书/坏私钥/不匹配都有启动失败测试。
- [x] 无 TLS 配置时既有明文行为与测试完全不变。
- [x] 文件预算、依赖边界、fmt/clippy/test/doc/deny 全绿；无新传输栈、无新网络依赖。

## Out of Scope

- 证书热加载/轮换、文件监听、零停机换证。
- mTLS / 客户端证书校验、SNI 多证书/证书选择。
- 指标监听器 TLS、管理面 TLS。
- OCSP/CRL、TLS 1.0/1.1、QUIC/HTTP/3。
- 完整 HTTP/2 请求 e2e（只验证 TLS ALPN 协商；既有的 H2C/HTTP 数据面测试不变）。
- 上游 TLS（已由 `11-11-upstream-tls` 交付）。

## Deliverables

- `nico-http`：`InboundTls`、`validate_server_identity`、`HttpSettings` 入站 TLS、TLS listener 构建、`ServiceBuildError`、`x-forwarded-proto` 来源。
- `nico-server`：`http.tls` bootstrap 段、证书/私钥校验、`serve_with` 的 service build 失败路径、TLS e2e 与 fixtures 复用。
- 文档：`config/bootstrap.yaml`、`README` 当前限制、`pingora-guidelines.md`、`error-handling.md`、`logging-guidelines.md`。
- tests：设置/校验单元、bootstrap 失败、真实 TLS 客户端 e2e、ALPN、明文拒绝。
