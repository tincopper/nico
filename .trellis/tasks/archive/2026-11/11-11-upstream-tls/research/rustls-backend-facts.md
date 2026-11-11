# 上游 TLS 取证：pingora 0.9.1 的 rustls 后端

> 取证方式：读 `pingora-core-0.9.1` / `pingora-error-0.9.1` / `pingora-proxy-0.9.1` 源码，以及
> `cargo info pingora-rustls`（0.9.1，已下载到本地 registry，工作树与 `Cargo.lock` 未改动）。
> 每条结论都带证据位置，实现时按这些位置复核。

## 1. 现状：本构建没有 TLS 后端，且这是显式选择

| 事实 | 证据 |
|---|---|
| `nico-http` 只启用 pingora 的 `proxy` feature | `crates/nico-http/Cargo.toml` |
| https 目标在连接前被拒绝，理由是"宁可失败也不明文降级" | `crates/nico-http/src/upstream.rs:118-155`（`refuse_unavailable_tls`）、常量 `TLS_BACKEND_ENABLED = false` 在 `:47` |
| 拒绝行为有 3 条测试 | `crates/nico-http/src/upstream/tests.rs:79`、`:143`、`:168` |
| spec 已把该行为写死 | `.trellis/spec/backend/pingora-guidelines.md:255`（`scheme: https` → 502）、`:388`（noop_tls 是 pass-through） |
| 未开 feature 时 pingora 编入的是空壳 | `pingora-core-0.9.1/src/protocols/tls/noop_tls/mod.rs`（`TlsRef` 空结构 + pass-through stream） |

## 2. 后端选择是 Cargo feature，默认全关

`pingora-core-0.9.1/Cargo.toml [features]`：

```
default = []
openssl   = ["pingora-openssl", "openssl_derived"]
boringssl = ["pingora-boringssl", "openssl_derived"]
s2n       = ["pingora-s2n", "any_tls", "dep:x509-parser", "ouroboros", "lru"]
rustls    = ["pingora-rustls", "any_tls", "dep:x509-parser", "ouroboros"]
```

Cargo feature 在全图是 unified（additive）的：任何 crate 打开 `pingora/rustls`，整个构建都有 TLS。
"哪个产物能连 https"因此是产物属性，不是某个中间 crate 的属性。

## 3. `pingora-rustls 0.9.1` 的实际形态

| 事实 | 证据 |
|---|---|
| rustls `0.23.12`，features `ring` + `logging` + `std` + `tls12`，`default-features = false` | `pingora-rustls-0.9.1/Cargo.toml` |
| crypto provider 是 **ring**（不是 aws-lc-rs）；`install_default_crypto_provider()` 由 pingora 在构建 connector 时调用 | `pingora-rustls-0.9.1/src/lib.rs`；`pingora-core/src/connectors/tls/rustls/mod.rs:66` |
| TLS 1.2 与 1.3 都启用，固定 `[&TLS12, &TLS13]` | `connectors/tls/rustls/mod.rs:96`、`:185` |
| 根证书：`rustls-native-certs` 读平台库，且**尊重 `SSL_CERT_FILE` / `SSL_CERT_DIR`** | `pingora-rustls/src/lib.rs` 的 `load_platform_certs_incl_env_into_store` |
| 自定义 CA：`ConnectorOptions.ca_file`（进程级）或 `PeerOptions.ca`（每 peer） | `connectors/mod.rs:75`、`connectors/tls/rustls/mod.rs:79`、`:142` |
| **两者都是"替换"根集，没有"追加"语义** | `connectors/tls/rustls/mod.rs:76-91`（`if let Some(ca_file) { load_file } else { platform }`） |
| 关闭校验：`verify_cert` / `verify_hostname` 被真正读取 | `connectors/tls/rustls/mod.rs:265-271` → `connectors/http/mod.rs:616 apply_no_verify` |
| mTLS：`ConnectorOptions.cert_key_file`（默认客户端证书）+ `PeerOptions.client_cert_key` | `connectors/mod.rs:83-90`、`connectors/tls/rustls/mod.rs:90` |
| 许可：`rustls` Apache-2.0/MIT/ISC；`ring 0.17.14` = `Apache-2.0 AND ISC` | `cargo info ring`；两者都已在 `deny.toml` 的 `allow` 列表 |
| 构建代价：`ring` 含 C/汇编，需要 C 编译器（`cc`）；不需要 cmake / ninja | ring 0.17 的 build script；相对 boringssl 的差异 |

## 4. peer 参数：策略面已现成

`pingora-core-0.9.1/src/upstreams/peer.rs`：

| 字段 | 默认 |
|---|---|
| `verify_cert` | `true`（`:582`） |
| `verify_hostname` | `true`（`:583`） |
| `alpn` | `ALPN::H1`（`:587`） |
| `ca` | `None`（`:588`） |
| `client_cert_key` | `None` |

`nico-http` 现在只设 `connection_timeout` / `read_timeout`（`crates/nico-http/src/upstream.rs:183-197`）。

## 5. connector 在"构建 service 时"生成，不是每连接

| 事实 | 证据 |
|---|---|
| `ProxyServiceBuilder::build()` 用传进来的 `ServerConf` 建 client connector | `pingora-proxy-0.9.1/src/lib.rs:222` `Connector::new(Some(ConnectorOptions::from_server_conf(&conf)))` |
| `ConnectorOptions::from_server_conf` 读 `server_conf.ca_file` | `pingora-core/src/connectors/mod.rs:117,137-138` |
| `TransportConnector::new` 立即构建 TLS connector（`tls_ctx: tls::Connector::new(options)`） | `pingora-core/src/connectors/mod.rs:188-212` |
| nico 的装配顺序：先 `Server::new(None)`，再替换 `server.configuration`，最后才 `http_service(&server.configuration, …)` | `crates/nico-server/src/server.rs:95`、`:105`、`:154` |
| `server_conf()` 已经是集中构造点 | `crates/nico-http/src/service.rs:49-58` |

**推论**：`ServerConf.ca_file` 在 nico 现有装配顺序下是会生效的（conf 在 service 构建前替换）。
代价是 **CA 在进程启动后不可热改**：reload 只换文档快照，不重建 service。

## 6. 错误分类

`pingora-error-0.9.1/src/lib.rs` 有 `TLSHandshakeFailure`、`TLSHandshakeTimedout`、`InvalidCert`、
`TLSWantX509Lookup`、`HandshakeError`。rustls 路径把握手失败统一成 `TLSHandshakeFailure`
（`connectors/tls/rustls/client.rs` 的 `handshake()`）。

nico 现有的超时谓词只认 `ReadTimedout | WriteTimedout | ConnectTimedout`
（`crates/nico-http/src/proxy/failure.rs:94`）——**`TLSHandshakeTimedout` 不在其中**，会被当成普通
连接失败（502）。

## 7. 对照物：aiway 怎么做（为什么 nico 不照搬）

| 事实 | 证据 |
|---|---|
| aiway 的 pingora **没有编译任何 TLS 后端** | `aiway/gateway/Cargo.toml:15`（`features = ["pingora-proxy","proxy","lb"]`） |
| 它的 LLM 流量根本不走 pingora：`request_filter` 截 `/model/`，转给 `reqwest::Client` | `aiway/gateway/src/service.rs:55`、`model_proxy/proxy/client.rs:26`、`proxy.rs:56` |
| 因此它的 https 上游代理（若配了）走的是 noop stub，明文 | `gateway/src/gateway.rs:348` `build_http_peer`（只设 `is_tls`，无 provider） |
| 它的 reqwest 默认 TLS = rustls | `reqwest-0.13/Cargo.toml`：`default-tls = ["rustls"]` |
| 入站 TLS 在独立 `access` 二进制里用 openssl | `aiway/access/src/cert.rs:8-9` |

结论：aiway 用"两套栈"绕过问题（数据面明文 + LLM 走 reqwest）。nico 的 LLM 路由就在数据面里，
照搬等于在同一进程里引入第二套 HTTP 栈、超时、取消与流模型，与既有"单栈"纪律冲突
（`deny.toml` 与 `metrics-exporter-prometheus` 关掉 hyper/tokio 的理由同源）。

## 8. 尚未定论、实现时必须现场验证的两点

1. `peer.options.ca` 的构造：`CaType = [WrappedX509]`，`WrappedX509` 是 `self_referencing(pub_extras)`
   结构，`new(raw_cert, parse_x509)` 由 `pub_extras` 导出，`parse_x509` 是 `pub fn`
   （`pingora-core/src/utils/tls/rustls.rs:106-118`）。**本里程碑不使用它**，但它决定了"每 provider
   一份 CA"是否可行，实现时写一条探针确认。
2. rustls 路径没有 ALPN H2 的上游默认；`PeerOptions.alpn` 默认 `H1`。上游 HTTP/2 需要显式设
   `ALPN::H2` 并确认 pingora 的 h2 client 路径可用——**不在本里程碑**。

## 9. 实测：平台信任库不可读时是 panic，不是启动错误

实现第 1–4 步后在本机（macOS，沙箱/非图形会话）实测：

```
$ cargo test -p nico-server --test ops_surface
thread 'nico-test-server' panicked at pingora-core-0.9.1/src/connectors/tls/rustls/mod.rs:50:51:
called `Result::unwrap()` on an `Err` value:
  Error { etype: InvalidCert, ... cause: Error { code: -36, message: "I/O error." },
          context: "Failed to load native certificates" }
```

即：未配置 `ca_file` 时 pingora 的 `tls::Connector::new`（`:50` 的 `build_connector(...).unwrap()`）
直接 abort。`rustls-native-certs 0.7.3` 的 `load_native_certs()` 在该进程里返回 `errSecIO (-36)`。

推论（已写进 `prd.md` R9 / `design.md` §3.3）：未配置 `ca_file` 时，启动期必须自己探一次平台信任库；
否则默认路径在任何无法读平台信任库的环境（CI、SSH 会话、沙箱、缺少 ca-certificates 的容器）里都是
建 service 时崩溃，而不是有名有姓的启动失败。

## 10. 依赖版本的实测偏差

- rustls 实际解析为 **0.23.46**（不是 `pingora-rustls` 声明的下界 0.23.12）：0.23.38 触发 4 条
  RUSTSEC（2026-0285 / 2026-0346 / 2026-0348 / 2026-0349，要求 ≥ 0.23.45/46）。0.23.46 仍在
  `^0.23.12` 范围内，全图仍只有一版。
- `x509-parser → asn1-rs` 带入 `time`，0.3.45 命中 RUSTSEC-2026-0009；已抬到 0.3.55。
  **副作用**：advisory-free 的 `time` 声明 `rust-version = 1.88`，工作区声明 1.85。
- `RUSTSEC-2025-0134`（rustls-pemfile 不再维护，解析代码已迁入 `rustls-pki-types`）在
  `deny.toml` 记录为 ignore：它是 `pingora-rustls` 的非可选依赖，且只解析启动期本地文件。

