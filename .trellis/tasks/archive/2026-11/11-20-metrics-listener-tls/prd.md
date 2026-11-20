# metrics 监听 TLS 终止

## Goal

让 bootstrap 的 `metrics` 段可选地终止 TLS：`/health/live`、`/health/ready`、`/metrics` 三个端点
可以只经加密连接暴露，而不再必须依赖前置代理。身份读取、配对校验与 TLS 参数构建与数据面共用同
一份实现，启动期定型、失败拒绝启动、变更需重启。

## Background

自报面是独立的 Pingora service，在 `metrics.listen` 上 `add_tcp`，只有明文。数据面已经有完整的
单证书入站 TLS：`nico_http::InboundTls` 承载路径、`validate_server_identity` 做启动期配对、
`service::tls_settings` 构建 Pingora 的 `TlsSettings`（静态 resolver + ALPN）。这些目前是数据面
私有路径，metrics 想复用就必须把它变成有第二个真实消费者的公共入口，而不是复制一份。

"metrics 也走 TLS" 是部署侧的合规需求：把 health 端点交给外部探针、把 exporter 交给 Prometheus
时，很多环境不允许明文口。它与数据面的 mTLS、SNI 多证书、证书热加载是**不同**的能力，见范围外。

## Requirements

### R1｜schema：`metrics.tls` 是可选的入站身份

`metrics` 段保持单地址（不改成 `listeners[]`），新增可选 `tls` 块：

```yaml
metrics:
  listen: 127.0.0.1:9090
  tls:                       # 有则终止 TLS；省略即明文（今天的行为）
    cert_file: /etc/nico/fullchain.pem
    key_file: /etc/nico/key.pem
```

`tls` 块与 `http.listeners[].tls` 形状相同、字段名相同，两个方向仍是两件事：入站身份 vs
`proxy.tls` 的上游信任根。

### R2｜启动期校验，失败拒绝启动

- 写了 `tls:` 但 body 为空（或缺少任一文件）是配置错误，**不得**退回明文。
- 两个文件在启动时读取并配对校验；读不到、不是 PEM、不配对都让进程拒绝启动，错误字段为
  `metrics.tls.cert_file` / `metrics.tls.key_file`。
- `tls` 块内的未知字段被拒绝（`deny_unknown_fields`）。
- `metrics.listen` 的既有规则不变：必填、合法监听地址、与任何 `http.listeners[]` 地址不同。

### R3｜服务行为

- 带 `tls` 时该监听只提供 TLS：明文请求在握手阶段被拒，不产生 HTTP 响应。
- ALPN 只声明 `http/1.1`：抓取器与探针都是 HTTP/1.1，自报面不引入第二种协议；数据面的
  `h2`/`http/1.1` 协商行为不变。
- 三个端点的状态码、body、头集合与明文时完全一致；TLS 只是传输事实。
- `metrics` 段仍与数据面完全分离：文档里的路由无法遮蔽它，reload 不改变它。

### R4｜一份实现，两个消费者

- 入站身份的读取/配对校验与 `TlsSettings` 构建只在 `nico-http` 有一份定义；数据面与自报面各调用
  一次，参数只表达 ALPN 选择。
- 不复制 ALPN、resolver、协议版本策略到 `nico-server`。

### R5｜范围外

- 客户端证书认证（mTLS）、SNI 多证书、证书热加载/轮换（与数据面同样需要重启）。
- `metrics.listeners[]`（多地址）、同一端口自动识别明文与 TLS、自报面的 h2/h2c。
- 数据面入站 TLS、`proxy.tls` 信任根、任何路由/插件语义。

### R6｜文档与契约同步

- `README.md`：bootstrap 示例与要点、可观测性一段说明 TLS 是可选传输；不再出现"自报面只能明文"的
  表述。
- `config/bootstrap.yaml` 的 `metrics` 注释、`deploy/bootstrap.yaml` 的示例注释。
- `docs/nginx-migration.md` 中把 metrics 描述为明文的段落。
- `.trellis/spec/backend/directory-structure.md` 的 bootstrap 文件清单与 metrics 传输描述。

### R7｜兼容与回滚

- 省略 `tls` 的 bootstrap 与今天的解析、启动、服务行为完全一致；数据面代码路径除新增参数外不变。
- 回滚是纯配置操作（删掉 `tls` 块并重启），不涉及数据迁移或 LKG 缓存格式。

## Acceptance Criteria

- [x] AC1（R1/R2）：bootstrap 解析测试覆盖——合法配对通过；空 `tls:`、缺 `key_file`、未知字段分别
      被拒且字段名为 `metrics.tls.*`；地址冲突规则与既有测试一致。
- [x] AC2（R3）：E2E——用仓库 TLS fixture 起一个带 `metrics.tls` 的进程，TLS 客户端能取到
      `/metrics` 与 `/health/ready`；同一端口上的明文请求在握手阶段失败、拿不到 HTTP 响应；客户端
      同时提供 `h2` 与 `http/1.1` 时协商结果是 `http/1.1`。
- [x] AC3（R3）：既有明文自报面场景（`metrics_startup`、`ops_surface`）不改断言即通过；数据面
      双 listener 与 TLS 场景（`tls_inbound`）不回归。
- [x] AC4（R4）：`TlsSettings` 构建只有一处定义，两个调用点；`nico-server` 不直接使用
      Pingora 的 `TlsSettings`/`ALPN` 类型。
- [x] AC5（R6）：文档与 spec 中不再有"metrics 监听只有明文"的断言；bootstrap 示例可直接发布。
- [x] AC6：`cargo fmt --check`、`cargo clippy --workspace --all-targets --all-features -D warnings`、
      `cargo test --workspace --all-features`、`RUSTDOCFLAGS="-D warnings" cargo doc` 全绿；
      改动文件不超过 500 行预算。

## Notes

- 交付单元是"bootstrap 的一个可选块 + 一条复用后的构建路径 + 一个传输测试"，不需要父/子任务拆分。
