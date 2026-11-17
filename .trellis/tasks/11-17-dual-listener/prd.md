# HTTP 与 HTTPS 双 listener 并存

## Goal

让网关进程同时服务明文 HTTP 与 HTTPS：`http.tls` 增加可选的独立 `listen` 地址，配置后一个 Service 注册明文与 TLS 两个 listener；`x-forwarded-proto` 从进程级标志改为**按连接**判定。单 listener 的既有语义保持不变。

## Background

- 当前（`11-17-inbound-tls` 交付后）是**单 listener 二选一**：`service()` 配置了 `http.tls` 就 `add_tls_with_settings` 接管 `http.listen`，否则 `add_tcp`；`http.h2c + http.tls` 被拒绝（同一 listener 不能既是明文又是 TLS）。
- `x-forwarded-proto` 的判定来自进程级 `HttpApp.inbound_tls: bool` 标志。双 listener 下明文连接会被误标 `https`，必须改为按连接判定。
- Pingora 0.9.1 提供按连接的 TLS 信息：`Session::digest()` → `Digest.ssl_digest: Option<Arc<SslDigest>>`（`protocols/digest.rs:31`，TLS 连接才是 `Some`）；Service 本身支持多 listener 注册（`add_tcp` 与 `add_tls_with_settings` 可并存）。
- 证书/私钥的启动期校验（`validate_server_identity`、`bootstrap/http/tls.rs`）与本任务无关的部分全部复用。

## Requirements

### R1：bootstrap 配置

- `http.tls` 增加可选 `listen` 字段（地址，格式同 `http.listen`）；`cert_file`/`key_file` 语义不变。
- `http.tls.listen` 缺省 → **现语义不变**：TLS 接管 `http.listen`，整个进程只有一个 listener。
- `http.tls.listen` 存在 → 双 listener：`http.listen` 保持明文，`http.tls.listen` 终止 TLS；两地址必须不同（相同是配置错误，字段 `http.tls.listen`）。
- `http.h2c: true` 在双 listener 模式下**允许**（作用于明文 listener）；仅当 TLS 接管 `http.listen` 时保留现有冲突拒绝。
- 空的 `http.tls:`（无字段）仍是配置错误；未配置 `http.tls` 时行为与今天完全一致。

### R2：listener 注册

- 双 listener 模式下 `service()` 同时注册 `add_tcp(http.listen)` 与 `add_tls_with_settings(http.tls.listen, ...)`，共享同一套 HTTP hooks、插件链、路由与优雅停机。
- 两个 listener 的 drain/grace 配置一致（进程级，不变）。

### R3：按连接的传输判定

- `x-forwarded-proto`（客户端未提供时）：按当前连接判定——`ssl_digest` 为 `Some` 写 `https`，否则 `http`；不追加第二个值。
- 进程级 `HttpApp.inbound_tls` 标志移除或退化为 fallback；当 `digest()` 不可判定（如部分 H2 场景）时按 listener 配置语义回退，不允许猜测为 `https`。
- 单 listener 模式（无论 TLS 或明文）的行为与今天一致。

### R4：启动期校验

- 证书/私钥校验路径完全复用（读文件 → `validate_server_identity` → `CertifiedKey`），失败是启动失败，错误字段指向 `http.tls.*`，不打印密钥材料。
- 新增校验：双 listener 模式下两地址解析后不得相同；`http.tls.listen` 为空串/端口 0 按既有地址校验规则拒绝。

### R5：测试

- 双 listener e2e：明文客户端 → 明文口 → 200 且上游见 `x-forwarded-proto: http`；TLS 客户端 → TLS 口 → 200 且上游见 `https`；明文打 TLS 口无 HTTP 响应；TLS 握手字节发明文口无 HTTP 响应。
- ALPN：TLS listener 协商 `h2`/`http/1.1`（复用现有客户端 fixture）；明文口 h2c 按 `http.h2c` 配置生效。
- bootstrap：`listen` 相同拒绝；`h2c + 双 listener` 通过；`h2c + 接管模式` 拒绝；缺省 `listen` 的接管语义回归测试不变。
- 既有单 listener 测试（明文与 TLS 接管）零改动通过。

## Acceptance Criteria

- [ ] `http.tls.listen` 可配置；缺省时接管语义与今天完全一致（既有测试零改动）。
- [ ] 双 listener 下明文与 TLS 同时服务，`x-forwarded-proto` 按连接正确（http/https 各得其所）。
- [ ] 传输判定来自连接本身（`ssl_digest`），进程级标志不再单独决定 `x-forwarded-proto`。
- [ ] `h2c` 在双 listener 下可用；两地址相同、`h2c + 接管` 有启动失败测试。
- [ ] 证书校验失败仍拒绝启动且不泄露密钥材料。
- [ ] fmt/clippy/test/doc/deny 全绿；文件预算与依赖边界（不新增传输栈）守住。

## Out of Scope

- 证书热加载、mTLS、SNI 多证书、指标监听 TLS（沿 `11-17-inbound-tls` 的边界）。
- 每 listener 独立的 drain/grace、独立的插件链或路由。
- HTTP/3 / QUIC、h2c 与 TLS 在**同一端口**的自动识别。
- 上游 TLS（`11-11-upstream-tls` 已交付，不动）。

## Deliverables

- `nico-http`：`HttpSettings` 双地址与校验、`service()` 双 listener 注册、按连接的传输判定（hooks）。
- `nico-server`：`http.tls.listen` bootstrap 解析与校验、双 listener e2e 与 bootstrap 测试。
- 文档：`config/bootstrap.yaml` 示例、`README` 当前限制、`pingora-guidelines.md`（多 listener + 按连接传输判定）、`error-handling.md`（新错误字段）。
