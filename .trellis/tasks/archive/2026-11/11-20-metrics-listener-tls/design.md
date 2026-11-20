# 设计：metrics 监听的 TLS 终止

## 1. 不变量与失败模式

先定不该被破坏的事实，再选结构。

1. **自报面与数据面是两个 service。** `/metrics` 不经过代理 hook 链，文档无法遮蔽它；TLS 不能
   把两者合并——合并会让自报面变成一条可被文档影响的路由。
2. **身份在启动期定型。** 证书/密钥只在启动时读一次并配对校验；运行期不重读、不热加载。坏文件
   必须是可读的启动错误，不能是框架内部的 panic。
3. **明文是缺省，TLS 是显式。** 省略 `tls` 与今天完全一致；写了 `tls:` 就不允许静默退回明文。
4. **一个地址。** `metrics.listen` 是单值；多地址需求没有出现，`listeners[]` 化会引入与数据面
   `listeners[]` 无谓的对称性负担。
5. **入站 TLS 的语义只有一份。** 协议版本、resolver、ALPN 的选择属于 `nico-http`；`nico-server`
   只决定"这个监听要不要 TLS、ALPN 选哪种"。

失败模式与对应行为：

| 失败 | 行为 |
|---|---|
| `tls:` 存在但空 / 缺文件 | 启动失败，字段 `metrics.tls.cert_file` / `metrics.tls.key_file` |
| 文件不可读 / 非 PEM / 不配对 | 启动失败，字段同上，消息不含任何 PEM 或密钥字节 |
| `tls` 块有未知字段 | 解析失败（`deny_unknown_fields`） |
| 端口绑定失败 | 启动失败（既有行为，preflight bind） |
| 构建 `TlsSettings` 失败 | 启动失败，进程不进入服务状态 |
| 客户端明文连 TLS 端口 | 握手阶段关闭连接，不产生 HTTP 响应 |

## 2. 契约

### 2.1 bootstrap schema

```yaml
metrics:
  listen: 127.0.0.1:9090     # 必填（段存在时），合法监听地址，且与 http.listeners 全不相同
  tls:                       # 可选；存在即终止 TLS
    cert_file: /etc/nico/fullchain.pem   # 必填（块存在时）
    key_file: /etc/nico/key.pem          # 必填（块存在时）
```

YAML 语义细节：`tls:` 后为空 body 必须被识别为"存在但缺文件"，而不是"未配置"。这与
`http.listeners[].tls` 的处理一致，且是同一个反序列化函数。

### 2.2 传输

- 协议版本、cipher 套件：与数据面完全相同（`TlsSettings::intermediate` + rustls 默认 provider）。
- 证书解析：静态 resolver 持有已校验的 `CertifiedKey`，SNI 被忽略（单证书）。未来多证书替换的是
  resolver，不是监听管线。
- ALPN：`http/1.1`。数据面用 `h2,http/1.1`，自报面只声明 h1——抓取器与探针不需要 h2，多一种
  协议只是多一条未经测试的路径；测试钉住"客户端同时提供 h2/h1 时协商结果是 h1"。
- 明文连入 TLS 端口的失败发生在 rustls 握手里，`ServeHttp` 不会看到请求。

### 2.3 错误字段

| 场景 | 字段 |
|---|---|
| 段存在但无 `listen` | `metrics.listen`（既有） |
| 地址非法 / 与数据面冲突 | `metrics.listen`（既有） |
| `tls` 块存在但无 `cert_file` | `metrics.tls.cert_file` |
| `tls` 块存在但无 `key_file` | `metrics.tls.key_file` |
| PEM 解析失败 / 不配对 | `metrics.tls.cert_file` 或 `metrics.tls.key_file`（由失败侧决定） |
| 服务构建时二次读取失败 | `ServiceBuildError`（`nico-http` 的既有错误面） |

## 3. 结构

### 3.1 模块改动

```
crates/nico-server/src/bootstrap/
  inbound_tls.rs        # 新增：RawTlsIdentity + section 反序列化 + resolve(字段前缀)
  http.rs               # 改：listeners[] 解析调用 inbound_tls::RawTlsIdentity.resolve("http.listeners[i].tls")
  http/listener.rs      # 删除：内容并入 inbound_tls.rs（唯一实现，两个使用点）
  metrics.rs            # 改：RawMetrics 增加 tls；MetricsSettings 持有 Option<InboundTls>
  metrics/tests.rs      # 改：TLS 解析正负例

crates/nico-http/src/
  service.rs            # 改：tls_settings -> pub listener_tls(identity, ListenerAlpn)；数据面传 Http2Or1
  lib.rs                # 改：导出 listener_tls 与 ListenerAlpn

crates/nico-server/src/
  metrics/listener.rs   # 改：service() 可失败；有 tls 时 add_tls_with_settings(.., listener_tls(.., Http1))
  server.rs             # 改：处理 metrics service 构建失败（与数据面同样的启动失败出口）
```

`inbound_tls.rs` 的 `resolve` 接收**字段前缀**而不是索引：`http.listeners[{i}].tls` 与
`metrics.tls` 是两种命名，索引属于 section 所有者的知识；把前缀作为参数让两个调用点各自拼出
自己的路径，而不是在共享模块里分支。

### 3.2 `nico-http` 的公共入口

```rust
/// Which protocols a TLS listener offers over ALPN.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenerAlpn {
    /// HTTP/1.1 only: the self-reporting surface.
    Http1,
    /// HTTP/2 preferred, HTTP/1.1 allowed: the data plane.
    Http2Or1,
}

pub fn listener_tls(
    identity: &InboundTls,
    alpn: ListenerAlpn,
) -> Result<TlsSettings, ServiceBuildError>;
```

- 返回值是 Pingora 类型，但 `nico-http` 的公共面已经返回 `Service<…>` / `ServerConf`，没有新增边界
  泄漏；调用方用类型推断，不必 import 任何 Pingora 类型（AC4）。
- 参数用本 crate 的 enum 而不是 `bool`：`enable_h2()` 与"不声明 ALPN"是两种不同结果，布尔参数
  会把 `Http1` 表达成"什么都不声明"，测试也无法把"只声明 h1"与"没有 ALPN"区分开。
- 数据面现有的 `tls_settings(tls)` 变成 `listener_tls(tls, ListenerAlpn::Http2Or1)`，行为逐字节不变。

### 3.3 服务装配

```rust
pub(crate) fn service(...) -> Result<Service<HttpServer<MetricsApp>>, ServiceBuildError> {
    let mut service = Service::new("metrics".to_string(), app);
    match settings.tls() {
        Some(identity) => {
            let tls = nico_http::listener_tls(identity, ListenerAlpn::Http1)?;
            service.add_tls_with_settings(settings.listen(), None, tls);
        }
        None => service.add_tcp(settings.listen()),
    }
    Ok(service)
}
```

`server.rs` 在装配点把 `Err` 记成启动失败并返回 `FAILURE_EXIT_CODE`——与数据面
`http_service(...)` 失败、`metrics::assemble(...)` 失败同一条出口，不引入第三种半启动状态。

### 3.4 启动顺序

`BootstrapConfig::load`（含 metrics TLS 的读取与配对）→ `assemble`（preflight bind + recorder）→
`metrics::service`（二次读取身份并构建 `TlsSettings`）。二次读取是刻意的：bootstrap 校验与构建之间
文件可能变，二次读取把变化变成可读的启动错误。TLS 校验发生在 bind 预检之后，preflight 与真正
bind 之间的窗口是既有取舍，本次不改变。

## 4. 测试策略

| 层 | 覆盖 | 位置 |
|---|---|---|
| 反序列化/校验 | 合法配对通过；空块/缺 key/未知字段被拒且字段名正确 | `bootstrap/metrics/tests.rs` |
| 复用回归 | `http.listeners[].tls` 的错误字段仍是 `http.listeners[i].tls.*` | 既有 `bootstrap/tests.rs` 断言不改 |
| 传输 E2E | TLS 取 `/metrics`、`/health/ready`；明文握手失败；ALPN 协商 h1 | 新 `tests/metrics_tls.rs`（一个 server） |
| 不回归 | 明文自报面（`ops_surface`、`metrics_startup`）、数据面 TLS 双 listener | 既有测试不改断言 |

E2E 复用 `support/inbound_tls.rs::TlsClient` 与 `tests/fixtures/tls/` 的 `localhost` 证书 +
`test-root` 根；bootstrap 由 `support/http/tls_bootstrap.rs` 新增一个
`write_bootstrap_metrics_tls(directory, port, metrics_port, cert, key)`，与既有 renderer 同源，
避免第二份 bootstrap 模板。

## 5. 兼容、发布与回滚

- 无 `tls` 块：解析、启动、服务行为与今天一致；bootstrap 是本地文件，不被配置源服务，没有 LKG
  兼容问题（LKG 缓存的是 resources 文档）。
- 有 `tls` 块：新配置项，旧版本二进制会以"未知字段"拒绝启动——这是期望的失败方向（旧版本不会
  静默服务明文）。
- 回滚：删掉 `tls` 块并重启；没有数据迁移。
- 观测：启动日志 `metrics surface enabled` 增加传输事实（`metrics.tls = true/false`），与
  `metrics.listen` 并列；不新增指标。

## 6. 被否决的替代方案

| 方案 | 否决理由 |
|---|---|
| `metrics.listeners[]`（与数据面完全对称） | 没有多地址需求；单值与多值 schema 的迁移成本换不到能力，且会让"同一端口明文+TLS 识别"这类未实现能力看起来已存在 |
| `metrics.cert_file` / `key_file` 平铺 | 与 `http.listeners[].tls` 形状不一致，且无法表达"写了但缺一个"的失败 |
| 让 metrics 走数据面 listener 的 mTLS/ALPN 开关 | 自报面不是路由；共享配置会让文档或路由语义有机会影响运维面 |
| 复制 `tls_settings` 到 `nico-server` | 第二份 ALPN/resolver 策略；协议版本与 resolver 的演进会在两处漂移 |
| 只有 TLS、不再支持明文 | 破坏现有部署（compose 样例、探针），且明文 loopback 是合理默认 |
| 自报面启用 h2 | 无消费者；多一条未测试的协议路径，与"最小暴露面"相反 |
