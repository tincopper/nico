# 入站 TLS 技术设计

> 需求见 `prd.md`。范围是**单证书、启动期加载**的 rustls 终止；热加载、mTLS、SNI 多证书不在本任务。

## 1. 不变量

1. **TLS 是 listener 的身份，不是上游信任。** 顶层 `tls.ca_file` 继续只影响上游客户端；`http.tls` 只影响本进程的 listener。
2. **启动期失败，不是请求期 panic。** 证书/私钥在绑定前读取、解析并配对；坏文件拒绝启动，错误只报字段与原因。
3. **没有静默降级。** 配置 `http.tls` 后该 listener 只接受 TLS；明文字节不会进入 HTTP parser，也不会回落到 TCP。
4. **一个 listener 只有一种传输。** `http.tls` 与 `h2c: true` 冲突，发布/启动期拒绝，而不是忽略其中一个。
5. **HTTP 语义不变。** TLS 只替换 listener 的传输；路由、插件、LLM、预算、优雅停机、错误写出全部复用。
6. **数据面拥有代理事实。** `x-forwarded-proto` 由本跳的传输决定；客户端未提供时，TLS 写 `https`，明文写 `http`。

## 2. 类型与边界

### `nico-http`

```rust
// settings.rs / inbound TLS
pub struct InboundTls {
    cert_file: PathBuf,
    key_file: PathBuf,
}
impl InboundTls {
    pub fn from_bootstrap(cert_file: PathBuf, key_file: PathBuf) -> Self;
    pub fn cert_file(&self) -> &Path;
    pub fn key_file(&self) -> &Path;
}

impl HttpSettings {
    pub fn resolve(/* existing args */, upstream: TlsPolicy) -> Result<Self, HttpSettingsError>;
    pub fn resolve_with_inbound_tls(
        /* existing args */,
        upstream: TlsPolicy,
        inbound: Option<InboundTls>,
    ) -> Result<Self, HttpSettingsError>;
    pub fn inbound_tls(&self) -> Option<&InboundTls>;
}
```

- `resolve` 委托 `resolve_with_inbound_tls(..., None)`；既有调用点不变。
- `inbound.is_some() && h2c` → `HttpSettingsError` 字段 `h2c`，原因“TLS 与明文 h2c 不能在同一 listener”。
- `InboundTls` 只保存路径；它不读取文件，因此 `HttpSettings` 仍是纯值，仍可 `Clone/Debug/PartialEq`。

### 证书校验

```rust
pub fn validate_server_identity(cert_pem: &[u8], key_pem: &[u8])
    -> Result<(), ServerIdentityError>;
```

- `rustls_pemfile` 读证书链与私钥；至少一个证书；私钥类型必须是 rustls 支持的签名 key。
- `ServerConfig::builder().with_no_client_auth().with_single_cert(...)` 做最终配对；失败统一为 `ServerIdentityError`，`Display` 只描述规则，不包含 PEM/密钥。
- 该函数供 bootstrap 早期失败和 service 构建前复检共用。

### service 构建

`service()` 从 `Service<HttpProxy<HttpApp, ()>>` 变为：

```rust
pub fn service(...) -> Result<Service<HttpProxy<HttpApp, ()>>, ServiceBuildError>;
```

- 无 `http.tls`：与今天相同，`add_tcp(listen)`。
- 有 `http.tls`：读取/校验证书与私钥，构建一个 `StaticCertResolver`（`rustls::server::ResolvesServerCert`，持有 `Arc<CertifiedKey>`），通过 `TlsSettings::set_cert_resolver` 交给 Pingora；`enable_h2()` 通告 `h2`/`http/1.1`；`add_tls_with_settings(listen)`。
- 之所以用 resolver 而不是 `TlsSettings::intermediate(cert_path, key_path)`：后者的 `build()` 会重新读文件，并在文件变化/不匹配时 `panic!`。resolver 只持有启动期已校验的 `CertifiedKey`，`build()` 不再做文件 I/O。
- `ServiceBuildError` 变体覆盖文件读取、身份校验、`TlsSettings` 构建；`serve_with` 记录错误并返回失败退出码，不 panic。

### `nico-server` bootstrap

```yaml
http:
  listen: 0.0.0.0:8443
  tls:
    cert_file: /etc/nico/tls/fullchain.pem
    key_file: /etc/nico/tls/key.pem
# 顶层 tls 仍是上游信任根，未变
# tls:
#   ca_file: /etc/nico/roots.pem
```

- `RawHttp.tls: Option<RawHttpTls>`；空段（`tls:` 无字段）是错误。
- `RawHttpTls { cert_file: PathBuf, key_file: PathBuf }`；两个字段都必填。
- `RawHttp::resolve` 读取两个文件，调 `validate_server_identity`，通过后构造 `InboundTls`，再调 `HttpSettings::resolve_with_inbound_tls`。
- 字段路径错误统一为 `BootstrapError::InvalidField`：`http.tls.cert_file`、`http.tls.key_file`、`http.h2c`。
- `BootstrapConfig` 只存 `HttpSettings`，不存证书字节。

### `x-forwarded-proto`

- `HttpApp` 增加 `inbound_tls: bool`；`service()` 根据 `settings.inbound_tls().is_some()` 设置。
- `upstream_request_filter` 在客户端未提供 `x-forwarded-proto` 时写 `https`（TLS）或 `http`（明文）；已有值不追加第二个。
- TLS 终止后，`Host`/`x-forwarded-for`/`x-forwarded-host` 规则不变。

## 3. 数据流

```text
client --TLS--> [TlsSettings + static resolver] --HTTP/1.1 or h2--> Pingora
       -> request_filter (same routing/plugins)
       -> upstream request (x-forwarded-proto: https)
```

- TLS 握手在 Pingora 的 listener 内完成；HTTP parser 只看到解密后的字节。
- 握手失败不产生 `HttpContext`、不写请求记录、不进入 `fail_to_proxy`。
- `HttpApp` 的 `inbound_tls` 是进程级事实，因为一个 service 只有一种 listener 传输。

## 4. 校验与错误矩阵

| 条件 | 结果 |
|---|---|
| `http.tls` 缺失 | 明文 TCP，行为不变 |
| `http.tls` 空段 | 启动失败 `http.tls.cert_file`（“section present but no cert_file”） |
| 缺 `cert_file` 或 `key_file` | 启动失败对应字段 |
| 文件不可读 | 启动失败对应字段（不打印内容） |
| 证书 PEM 为空/坏/无证书 | 启动失败 `http.tls.cert_file` |
| 私钥 PEM 为空/坏/不支持 | 启动失败 `http.tls.key_file` |
| 证书与私钥不匹配 | 启动失败 `http.tls.key_file`（配对失败） |
| `http.tls` + `http.h2c: true` | 启动失败 `http.h2c` |
| 启动校验后文件消失/变化 | `service()` 复检失败 → `serve_with` 失败退出，不 panic |
| 客户端用错误根 | TLS 握手失败，无 HTTP 响应，无请求日志 |
| 客户端对 TLS 端口发明文 | TLS 握手失败/连接关闭，无 HTTP 响应 |
| 客户端 ALPN 只给 `h2` | 协商 `h2`（ALPN 测试） |
| 客户端无 ALPN | HTTP/1.1 路径可用 |

## 5. 测试

- `nico-http`：`InboundTls` 访问器、`HttpSettings` TLS/h2c 冲突、`validate_server_identity` 的空/坏/不匹配（fixtures 由 `nico-server` 的 TLS fixture 复用，或 `nico-http` 自己的 `#[cfg(test)]` PEM 常量）。
- `nico-server` bootstrap：`http.tls` 解析、空段、缺字段、坏文件、h2c 冲突；`BootstrapConfig` 保留路径；`serve_with` 的 service build 失败退出。
- e2e：真实 `tokio-rustls` 客户端；信任 `test-root.pem`；证书 `localhost.pem`/`localhost.key`；HTTP/1.1 请求到明文 mock upstream；断言 `200`、body、`x-forwarded-proto: https`；ALPN `h2` 协商；错误根握手失败；明文请求无 HTTP 响应。
- 既有明文测试全部保持。

## 6. 风险与取舍

| 风险 | 处置 |
|---|---|
| `TlsSettings::build` 会 panic | 用 static resolver 持有已校验 `CertifiedKey`，不再让它读文件/配对 |
| 文件在启动校验后变化 | `service()` 复检；失败退出，不做“尽力而为”的第一次请求 |
| 证书热加载需求 | 明确 out of scope；重启换证；单独任务再设计文件监听/零停机 |
| `h2c` 与 TLS 同 listener | 启动失败，而不是忽略 h2c |
| 客户端 `x-forwarded-proto` 伪造 | 本任务只在缺失时补；不覆盖调用方已有值（与现状一致）；可信代理链的伪造问题不在本任务 |
| mTLS/SNI 多证书 | out of scope；单证书 resolver 是当前唯一形态 |
| 指标/管理 listener TLS | out of scope；`metrics.listen` 不变 |
| 文件预算 | 新逻辑按 `settings/inbound.rs`、`service/tls.rs`、`bootstrap/http/tls.rs` 分文件，≤500 行 |

## 7. 交付边界

- `nico-http`：`inbound.rs`（`InboundTls`、`validate_server_identity`、`ServerIdentityError`）、`service.rs`（TLS listener、`ServiceBuildError`）、`settings.rs`、`proxy.rs`/`hooks/request.rs`（`inbound_tls` -> `x-forwarded-proto`）。
- `nico-server`：`bootstrap/http.rs` + `bootstrap/http/tls.rs`、`bootstrap.rs`、`server.rs`（`service` Result）。
- tests：`tests/support/http.rs` 的 `write_bootstrap_tls`、`tests/support/inbound_tls.rs` 客户端、`tests/tls_inbound.rs`；bootstrap 单元。
- 文档：`config/bootstrap.yaml`、`README.md`、`pingora-guidelines.md`、`error-handling.md`、`directory-structure.md`。
