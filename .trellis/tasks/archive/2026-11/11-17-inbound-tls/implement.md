# 入站 TLS 实施计划

> 设计见 `design.md`。顺序：先让 `nico-http` 能安全构建 TLS listener，再接 bootstrap，最后用真实 TLS 客户端验收。

## 1. `nico-http`：入站 TLS 值、校验与 listener

- [x] `src/inbound.rs`（或 `src/inbound/identity.rs`）：
  - `InboundTls { cert_file, key_file }`，`from_bootstrap`/访问器，`Debug` 只打印路径。
  - `validate_server_identity(cert_pem, key_pem) -> Result<(), ServerIdentityError>`：rustls-pemfile + rustls `ServerConfig::with_single_cert`；空/坏 PEM、无证书、坏/不支持 key、配对失败各有变体；`Display` 不含密钥/证书内容；`Send + Sync + 'static` 断言。
- [x] `settings.rs`：
  - `HttpSettings` 增加 `inbound_tls: Option<InboundTls>`、`inbound_tls()`。
  - `resolve_with_inbound_tls(...)`；`resolve` 委托 `None`；`h2c + tls` → `HttpSettingsError` 字段 `h2c`。
  - 既有 `HttpSettings::resolve` 测试不改行为；新测试覆盖访问器与冲突。
- [x] `service.rs`：
  - `service()` 改为 `Result<Service<...>, ServiceBuildError>`。
  - 有 TLS：读文件 → `validate_server_identity` → `CertifiedKey` + `StaticCertResolver` → `TlsSettings`（`set_cert_resolver`、`enable_h2`）→ `add_tls_with_settings`。
  - 无 TLS：`add_tcp` 不变。
  - `ServiceBuildError` 覆盖读文件、身份校验、TLS 设置构建；`Debug` 不打印证书。
- [x] `proxy.rs`：`HttpApp` 增加 `inbound_tls` 标志（`new` 默认 false，`set_inbound_tls` crate-private）；`service` 设置。
- [x] `proxy/hooks/request.rs`：`x-forwarded-proto` 缺失时按 `app.inbound_tls()` 写 `https`/`http`。
- [x] 测试：`InboundTls`/settings/service 的错误路径；`service()` 的既有测试加 `.expect`；文件 ≤500。

**检查点 A**：`cargo test -p nico-http`；`service` 构建失败可表达；无 TLS 配置的行为不变。

## 2. `nico-server`：bootstrap 与失败退出

- [x] `bootstrap/http.rs`：`RawHttp` 增加 `tls: Option<RawHttpTls>`；`RawHttpTls { cert_file, key_file }`，`deny_unknown_fields`；空段映射为缺失字段错误。
- [x] `bootstrap/http/tls.rs`：读两个文件、`validate_server_identity`、构造 `InboundTls`、转 `InvalidField`（`http.tls.cert_file`/`key_file`）；不打印内容。
- [x] `bootstrap/http.rs::resolve` 调 `HttpSettings::resolve_with_inbound_tls`；顶层 `tls` 仍是上游 `TlsPolicy`。
- [x] `server.rs::serve_with` 处理 `http_service` 的 `Result`：失败记录日志并返回 `FAILURE_EXIT_CODE`，不启动 serving。
- [x] `config/bootstrap.yaml`：加入 `http.tls` 注释示例；文档说明顶层 `tls` 仍是上游。
- [x] bootstrap 测试：合法 `http.tls`；空段；缺字段；不可读/坏证书/坏 key/不匹配；h2c 冲突；`BootstrapConfig::http().inbound_tls()`。
- [x] `serve_with` 的 service build 失败路径测试（不 panic、失败退出）。

**检查点 B**：`cargo test -p nico-server`；坏配置在启动期失败；`service` 构建失败有退出码。

## 3. 端到端与文档

- [x] `crates/nico-server/tests/support/http.rs::write_bootstrap_tls`（或等价）：写 `http.tls` 的 bootstrap；复用 `tests/fixtures/tls/{localhost.pem,localhost.key,test-root.pem,other-root.pem}`。
- [x] `tests/support/inbound_tls.rs`：`tokio-rustls` 客户端；可配 ALPN；读 `test-root.pem`；返回协商协议与响应；握手失败可断言。
- [x] `tests/tls_inbound.rs`（one server）：
  - TLS 客户端 + HTTP/1.1 请求 → 200、body、上游 mock 收到 `x-forwarded-proto: https`、路径正确。
  - 明文客户端连接 TLS 端口 → 握手失败/连接关闭，无 HTTP 响应。
  - 错误根客户端 → 握手失败，origin 无请求。
  - ALPN 只给 `h2` → 协商 `h2`；无 ALPN → HTTP/1.1 可用。
- [x] `README.md`：`http.tls` 配置、当前限制移除“只能明文”、说明启动期/重启换证。
- [x] `pingora-guidelines.md`：TLS listener 签名、错误矩阵、测试表；`error-handling.md`: 启动期 TLS 身份错误；`directory-structure.md`: `inbound.rs`/bootstrap TLS 树。
- [x] 文件预算、依赖边界（不新增传输栈；`tokio-rustls` 仅 dev）。

**检查点 C**：完整门 A/B；真实 TLS 握手通过；既有明文测试全绿。

## 4. 收尾

- [x] 更新 `task.json`/`implement.md` 检查点、PRD 验收勾选。
- [x] 不提交（除非用户明确要求）；报告真实门输出与已知缺口。

## 回滚点

- 第 1 步只改 `nico-http` 的配置/构建；无 TLS 配置时行为不变，可单独回退。
- 第 2 步改 bootstrap/assembly；失败退出路径可单独回退。
- 第 3 步只加测试/文档；可独立回退。

## 门

```bash
cargo +nightly fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features
cargo check --workspace --all-features
cargo check --workspace --no-default-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo deny check
find crates -name '*.rs' | xargs wc -l | awk '$1 > 500 && $NF != "total" {print}'
cargo tree -p nico-http --prefix none | grep nico-llm || true
```

## 落地记录（入站 TLS）

- `nico-http`：新增 `inbound.rs`（`InboundTls`、PEM 身份校验、静态证书 resolver）；`HttpSettings` 增加 `resolve_with_inbound_tls`/`inbound_tls`，TLS 与 `h2c` 冲突；`service()` 改为 `Result`，TLS listener 用启动期已校验的 `CertifiedKey` resolver（不让 Pingora 的 `TlsSettings::build` 再读文件/panic）；`HttpApp` 记录 listener 传输，`x-forwarded-proto` 在 TLS 下写 `https`。
- `nico-server`：`bootstrap/http/tls.rs` 读取并配对 `http.tls.cert_file`/`key_file`，空段/缺字段/坏文件/不匹配/h2c 冲突都有 `InvalidField`；`serve_with` 处理 `service` 的 `ServiceBuildError` 并返回失败退出码；顶层 `tls.ca_file` 仍是上游信任根。
- 测试：`nico-http` 372 + 2 doc；`nico-server` 188 / 1 ignored；workspace **1970 passed / 2 ignored / 77 suites**。新增 `nico-server/tests/tls_inbound.rs`（真实 `tokio-rustls` 客户端：HTTP/1.1 经 TLS、ALPN `h2`、错误根拒绝、明文无 HTTP 响应、上游看到 `x-forwarded-proto: https`），`src/bootstrap/tests/http_tls.rs`（合法/空段/缺字段/坏文件/不匹配/未知字段/h2c 冲突），`support/inbound_tls.rs`（客户端 fixture）。
- 门（真实输出）：`cargo +nightly fmt --all --check` 通过；`cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` 无 issue；`cargo test --workspace --all-features` 1970 passed / 2 ignored；`cargo check --workspace --all-features` 与 `--no-default-features` 通过；`RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` 通过；`cargo deny check` advisories/bans/licenses/sources 全 ok；文件预算无 >500 行；`cargo tree -p nico-http | grep nico-llm` 与 `-p nico-core` 的边界检查无输出。
- 边界：单证书、启动期加载；TLS 1.2/1.3、ALPN h2+h1；证书热加载、mTLS、SNI 多证书、指标监听 TLS、OCSP/CRL 不做；换证需重启。
- 未提交：按用户“实现”指令完成，工作区保留改动，等待提交指令。
