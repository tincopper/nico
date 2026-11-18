# bootstrap 配置对齐 nginx：`runtime` 段与 `http.listeners[]`

## Goal

bootstrap 配置面完成 nginx 对齐（结构对齐，非语法兼容），同时实现 HTTP 与 HTTPS 双 listener 并存：

1. **`runtime` 段**（nginx main 上下文的对应物）：`threads`、`grace_period_ms`、`graceful_shutdown_ms` 三个进程旋钮从 `http` 段迁出，归位到服务应用层。
2. **`http.listeners[]`**（nginx server 块的摊平）：一个条目 = 一个 listener，`tls` 块存在即 TLS listener；TLS-only / 纯明文 / 双 listener 由同一 schema 表达，`http.tls` 接管语义拆除。
3. **`x-forwarded-proto` 按连接判定**：`ssl_digest` 来自连接本身，删除进程级标志。

## Background

- nginx 调研结论（`research/nginx-config-analysis.md`）：进程事实与协议事实分层；listener 是一等公民，传输是它的属性；证书跟 listener 走。
- 项目已有对齐先例：`nico-lb` 策略词汇直接取自 nginx（`RoundRobin`/`SmoothWeighted`/`LeastConn`/`IpHash`）。
- 双 listener 的硬前提：传输判定必须来自连接本身。Pingora 0.9.1 提供 `Session::digest()` → `Digest.ssl_digest: Option<Arc<SslDigest>>`；H1/H2 的不可变 `digest()` 恒返回 `Some`（`protocols/http/server.rs:849`、`v2/server.rs:939-941`），判定可靠。

## Requirements

### R1：bootstrap 配置（`runtime` 段）

- `runtime` 段可选；缺席 = 全默认（threads 1、grace 5000、shutdown 30000）。
- 三个旋钮从 `http` 段迁出：`http.threads`/`http.grace_period_ms`/`http.graceful_shutdown_ms` 成为未知字段（`deny_unknown_fields` 拒绝）。
- 校验规则平移：threads `1..=256`；grace/shutdown `1..=300_000`；shutdown ≥ grace；错误消息引用常量。

### R2：`http` 段（`listeners[]`）

- `http` 段必填（serde required）；缺席 = serde Parse 错误。
- `http.listeners` 非空；条目 `listen` 必填、地址两两不同（解析后比较）。
- 条目 `tls` 块可选：存在即 TLS listener；`cert_file`/`key_file` 必填且同在。
- `http.h2c` 是段级事实：作用于全部明文条目；无明文条目时 `h2c: true` 拒绝。
- `http.tls`（接管语义）拆除：成为未知字段。

### R3：listener 注册与传输判定

- `service()` 遍历 `listeners`：`Plain` → `add_tcp`；`Tls` → 复检证书 + static resolver + `add_tls_with_settings`。
- 传输判定来自连接本身：`session.digest().ssl_digest.is_some()`；删除 `HttpApp.inbound_tls` 进程级标志。
- 明文客户端连 TLS 条目：握手失败，无 HTTP 响应，无请求记录。

### R4：启动期校验

- 证书/私钥在 bootstrap 读取并校验一次；`service()` 复检（文件可能在两次读取间变化）。
- 失败是启动失败（`BootstrapError::InvalidField` / `ServiceBuildError`），不是 panic；`service()` 失败 → `FAILURE_EXIT_CODE`。
- 错误只报字段与原因，不打印证书、私钥或密钥材料。

### R5：metrics 碰撞校验

- `metrics.resolve` 的碰撞对象从单一 data plane 地址改为**解析后的 listener 地址集合**。

### R6：上游方向重命名：`tls` → `proxy.tls`

- 顶层 `tls` 段（上游信任根）改为 `proxy.tls`：与 `http.listeners[].tls`（入站身份）拆名，对齐 nginx 的 `proxy_ssl_*` 家族（`proxy_ssl_trusted_certificate`）。
- 语义完全不变：`proxy` 缺席 = 平台信任根；`proxy.tls` 存在但空 = 错误（既有原则）；`ca_file` 存在 = 替换平台根（replace-not-append）。
- 错误字段路径：`tls.ca_file` → `proxy.tls.ca_file`。
- 顶层段集合收敛为：`runtime`（进程）、`http`（协议/入站）、`proxy`（出站）、`metrics`（自观测）、`config_source`（配置源）。
- `TlsPolicy` / `ensure_usable` 等内部类型名不变（非配置面）。

### R7：reload 管线旋钮归组到 `config_source`

- 顶层裸标量 `debounce_ms`、`max_wait_ms`、`cache_dir`、`allow_bootstrap_cache`、`schema_bounds` 迁入 `config_source` 段：它们全部是“配置获取与更新”子系统的行为事实，与 source 类型无关（local/etcd 语义一致）。
- 语义不变，只有字段路径加前缀：`config_source.debounce_ms` 等；旧顶层裸字段成为未知字段（`deny_unknown_fields` 拒绝）。
- `config_source` 段职责收敛为完整的配置获取子系统：从哪来（type/path/max_bytes）+ 怎么监听（watch）+ 怎么去抖（debounce/max_wait）+ 怎么兑底（cache）+ 文档边界（schema_bounds）。
- `BootstrapConfig` 内部字段与访问器不变（非配置面）；`RawConfigSource` 归组。

## Acceptance Criteria

- [x] `runtime` 段三旋钮生效，语义与今天等价（仅字段路径 `http.*` → `runtime.*`）；缺席 = 全默认。
- [x] `http` 段必填；`listeners` 非空；TLS 条目/明文条目/双 listener 同一 schema 表达；`http.tls` 成为未知字段。
- [x] 双 listener 下明文与 TLS 同时服务，`x-forwarded-proto` 按连接正确。
- [x] 传输判定来自 `session.digest().ssl_digest`，单测可构造 `SslDigest` 覆盖两分支。
- [x] 证书校验失败拒绝启动且不泄露密钥材料；`service()` 复检失败 → `FAILURE_EXIT_CODE`。
- [x] metrics 碰撞对解析后的地址集合校验。
- [x] 顶层 `tls` → `proxy.tls` 重命名完成，语义不变（缺席 = 平台根、空段 = 错误、`ca_file` 替换平台根）；入站/出站两个 `tls` 同名问题消除。
- [x] reload 旋钮归组 `config_source` 生效（语义不变，仅字段路径加前缀）；旧顶层裸字段被拒。
- [x] fmt/clippy/test/doc/deny 全绿；文件预算与依赖边界守住；无新传输栈。

## Out of Scope

- `resources.yaml` 的 nginx 对齐审查（routes≈location、upstreams≈upstream、plugins≈模块）——后续独立任务。
- nginx 迁移映射文档（`docs/nginx-migration.md`）——后续任务。
- 证书热加载、mTLS、SNI 多证书、指标监听 TLS（沿 `11-17-inbound-tls` 边界）。
- HTTP/3 / QUIC、同端口明文+TLS 自动识别。
- 上游 TLS（`11-11-upstream-tls` 已交付，不动）。

## Deliverables

- `nico-http`：`ServerSettings`（新 `server_settings.rs`）、`HttpSettings` 重造（`listeners` + `h2c`）、`Listener` 枚举、`service()` 多 listener 注册、`server_conf(runtime, tls)`、按连接传输判定、`Session::digest()` 复检路径。
- `nico-server`：`runtime` 段解析（`bootstrap/runtime.rs`）、`http.listeners` 解析（`bootstrap/http/listener.rs`）、`proxy` 段封装（`bootstrap/proxy.rs`，重命名上游信任根）、metrics 集合碰撞、`serve_with` 适配。
- 文档：`config/bootstrap.yaml` 示例、`README` 当前限制、`pingora-guidelines.md`（签名与契约表更新）、`error-handling.md`（错误矩阵更新）、`directory-structure.md`（文件归属更新）。
