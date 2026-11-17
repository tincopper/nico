# 设计：HTTP 与 HTTPS 双 listener 并存

## 决策 1：配置形状 —— `http.tls.listen`，缺省保持接管语义

`InboundTls` 增加 `listen: Option<String>`（私有字段 + 访问器，`Debug` 不新增敏感内容——地址本来就是配置）：

```yaml
http:
  listen: 0.0.0.0:8080      # 明文 listener（双模式下也是明文口）
  tls:
    listen: 0.0.0.0:8443    # 可选；缺省 = TLS 接管 http.listen
    cert_file: ...
    key_file: ...
```

- `listen` 缺省 → **接管模式**（现语义，既有测试零改动）。
- `listen` 存在 → **双 listener 模式**：`http.listen` 保持明文，`http.tls.listen` 终止 TLS。
- 选择挂在 `http.tls` 子段而非新顶层 `https` 段：证书、私钥、地址是同一个 listener 的身份事实，聚在一起；且与 R1 的"空段是错误"校验天然一致。

## 决策 2：三模式 listener 注册（`service.rs`）

```text
inbound_tls = None                      → add_tcp(http.listen)                     # 现状
inbound_tls.listen = None               → add_tls_with_settings(http.listen, tls)  # 现状
inbound_tls.listen = Some(addr)         → add_tcp(http.listen) + add_tls_with_settings(addr, tls)
```

- `HttpServerOptions.h2c` 不变地作用于明文 listener：明文模式与双模式的明文口按 `http.h2c` 生效；TLS 口的 HTTP/2 走 ALPN，`h2c` 标志对它无意义（Pingora 只对明文连接做 h2c 升级）。
- `h2c` 冲突规则改为**仅接管模式拒绝**（`HttpSettings` 层，现有错误字段 `h2c` 复用）：双模式下 h2c 与 TLS 分属两个 listener，不冲突。

## 决策 3：传输判定改为按连接，删除进程级标志

`upstream_request_filter` 改为：

```rust
let is_tls = session
    .digest()
    .and_then(|d| d.ssl_digest.as_ref())
    .is_some();
```

依据（pingora-core 0.9.1 源码核实）：

- H1：`Session::digest()` 恒返回 `Some`（`protocols/http/server.rs:849`）。
- H2：不可变 `digest()` 恒返回 `Some`（`v2/server.rs:939-941`；`digest_mut()` 才有多流限制）。
- `Digest.ssl_digest` 在连接建立时由底层流写入（`v1/server.rs:170`），TLS 连接才是 `Some`。

因此 `ssl_digest.is_some()` 在 H1/H2 下都是可靠的按连接判定；`digest()` 返回 `None`（subrequest/custom 路径，本构建未使用）防御性按明文处理。

**删除** `HttpApp.inbound_tls` / `set_inbound_tls` / `inbound_tls()`：双模式下它必然出错（明文连接被写 `https`），且按连接判定在单 listener 模式下行为等价——TLS listener 的每条连接 `ssl_digest` 都是 `Some`。

## 决策 4：地址相同校验放在 `HttpSettings` 层

与 h2c 冲突同层：`HttpSettings::resolve_with_inbound_tls` 在双模式下校验两地址不同。比较方式：两边都能 `to_socket_addrs` 解析时比较解析结果（容忍 `localhost:8080` vs `127.0.0.1:8080` 这类字面差异不在此处理——解析相同即拒绝）；任一侧解析失败沿用既有地址校验错误。bootstrap 层复用同一结果转 `InvalidField("http.tls.listen")`。

## 错误矩阵（新增/变化）

| 配置 | 结果 |
|---|---|
| `http.tls.listen` 与 `http.listen` 解析后相同 | 启动失败，`InvalidField("http.tls.listen")` / `HttpSettingsError` |
| `http.tls.listen` 地址非法（空串/端口 0） | 启动失败，既有地址校验规则 |
| `http.h2c: true` + 接管模式 | 启动失败（现状不变），字段 `h2c` |
| `http.h2c: true` + 双 listener | 允许，明文口生效 |
| 空 `http.tls:` | 启动失败（现状不变） |

## 测试矩阵

- 单元（`nico-http`）：三模式注册分支、地址相同、`h2c` 两模式、`InboundTls.listen` 访问器。
- bootstrap（`nico-server`）：合法双 listener、地址相同、`listen` 非法、`h2c` 双模式通过/接管拒绝。
- e2e（`tests/tls_inbound.rs` 扩展或新增）：双端口并存——明文口 HTTP/1.1 得 200 且上游见 `http`；TLS 口握手 + 请求得 200 且上游见 `https`；明文打 TLS 口无响应；ALPN h2 协商（同时证明按连接判定在 H2 下正确）。
- 回归：既有单 listener 测试零改动。

## 影响面与预算

- `nico-http`：`inbound.rs`（+`listen` 字段）、`settings.rs`（校验）、`service.rs`（三模式）、`proxy.rs`/`hooks/request.rs`（删标志，改判定）。无新文件或一个小测试文件；全部 ≤500 行。
- `nico-server`：`bootstrap/http.rs`/`http/tls.rs`（解析新字段）、`bootstrap/tests/http_tls.rs`、`tests/support/inbound_tls.rs`、`tests/tls_inbound.rs`。
- 无新依赖；`tokio-rustls` 仍 dev-only。
- 文档：`config/bootstrap.yaml`、`README`、`pingora-guidelines.md`、`error-handling.md`。
