# etcd 配置源 TLS 与认证

## Goal

让 `config_source.type: etcd` 能连接生产环境里真实存在的 etcd：TLS 端点（可选客户端证书）与用户名/
密码认证。信任根、客户端身份与口令全部在启动期定型，拼错或不可用就拒绝启动，绝不静默降级成明文。

## Background

etcd 源目前只接受明文 `http://` 端点：`config_source.endpoints` 的校验明确拒绝 `https://`，理由是
"this build's etcd client has no TLS feature"，README 的"当前限制"也把 TLS/认证凭据列为未暴露。
进程内 etcd mock（官方 rpc.proto 协议替身）只服务明文，所以这条路径连测试都无法覆盖 TLS。

底层能力是现成的：`etcd-client 0.14` 的 `ConnectOptions` 同时提供 `with_tls(ClientTlsConfig)` 与
`with_user(name, password)`，其 `tls` feature 走 `tonic/tls`（rustls 0.23 + tokio-rustls 0.26，与工作区
pin 的是同一栈、同一 crypto provider）。缺的是 nico 这一侧的 schema、启动期校验、装配与协议替身。

## Requirements

### R1｜schema：etcd 源的 `tls` 与 `auth` 块

两个块只属于 `etcd` 类型（写进 `local` 是未知字段，反之亦然）：

```yaml
config_source:
  type: etcd
  endpoints:
    - https://etcd-0.internal:2379
    - https://etcd-1.internal:2379
  key: /nico/resources.yaml
  max_bytes: 1048576

  tls:                                 # 可选；出现即 TLS
    ca_file: /etc/nico/etcd-ca.pem     # 块存在时必填：信任根，整体替换平台库
    # cert_file: /etc/nico/etcd-client.pem      # 可选，与 key_file 成对
    # key_file: /etc/nico/etcd-client-key.pem
    # domain_name: etcd.internal                # 可选：证书校验用的名字，缺省取端点主机名

  auth:                                # 可选；出现即鉴权
    user: nico
    password_ref: env:ETCD_PASSWORD    # 只接受 env: / file: 引用，不写明文
```

### R2｜传输一致性在启动期决定

- `https://` 端点**必须**配 `tls` 块；缺块是启动错误（字段点名该端点）。
- `tls` 块**必须**配全 `https://` 端点：带 `tls` 却给裸 `host:port` 或 `http://` 是启动错误
  （客户端在 TLS 配置下只连 HTTPS URL，猜一个 scheme 等于猜运维的意思）。
- 不带 `tls` 时现有规则不变：`http://` 与裸 `host:port` 照旧接受。
- 端点形状校验（空白、host、端口、端口 0）保持现状。

### R3｜启动期读取与校验，失败拒绝启动

- `ca_file`、`cert_file`、`key_file` 在 bootstrap 读取时打开：读不到、CA 不是可用证书集合、
  证书与私钥不配对，都是 `config_source.tls.*` 下的启动错误。
- `cert_file` 与 `key_file` 必须同时出现或同时省略。
- `password_ref` 是 `env:` / `file:` 引用；空值、不可读或不是引用都是启动错误。口令在构建 source
  时解析，**不进入**校验后的 bootstrap 值。
- 已发布的文档、LKG 缓存、数据面行为都不受影响：这次改动只在配置获取子系统。

### R4｜连接能力

- `GrpcEtcdClient` 能带 TLS 与凭据连接：CA 包、可选客户端身份、可选 `domain_name`、可选 user/password。
- 平台信任库**不参与** etcd 连接：信任根只能由 `ca_file` 明确给出（见 design 的取舍）。因此
  `ca_file` 在 `tls` 块内必填。
- 客户端身份与 CA 在构建 source 时再读一次（与入站 TLS 的二次读取同一理由），bootstrap 只持有路径。

### R5｜协议替身要能证明它

- 进程内 etcd mock 支持 TLS 与可选鉴权：TLS 用仓库 fixture（`localhost` 证书 + `test-root`），
  鉴权按 etcd 自己的语序实现——`Authenticate` 只在凭据正确时签发 token，KV/Watch 只接受该 token。
- 端到端至少覆盖：TLS 连接上的 get/watch 成功；错误口令在连接时得到 `EtcdError::Auth`，缺失凭据
  在第一次读取时得到 `EtcdError::Auth`；明文客户端连 TLS mock 失败（不产生明文降级）。

### R6｜范围外

- 平台信任库回退（不引入 `tonic/tls-roots`）、代理、用户名密码之外的 etcd 鉴权（token、OIDC）。
- etcd 客户端侧的证书热加载；配置源切换（local ↔ etcd）在运行期发生。
- 数据面 `proxy.tls`、入站 TLS、任何路由/插件语义。

### R7｜文档与 spec

- README：`config_source` 的 etcd 示例补 `tls`/`auth`，说明"信任根只来自 `ca_file`""口令用引用"，
  并更新"当前限制"里"etcd 的 TLS/认证凭据尚未暴露"这条。
- `.trellis/spec/backend/database-guidelines.md`（配置存储）与 `error-handling.md`（启动错误字段）
  同步；若 `bootstrap/source.rs` 触到 500 行预算，按边界拆分为 `source.rs` + `source/`。
- `dependency_boundaries.rs` 的 feature/栈断言随依赖变化更新。

## Acceptance Criteria

- [ ] AC1（R1/R2）：bootstrap 解析测试覆盖——https 无 `tls` 被拒；`tls` 配 http/裸端点被拒；
      未知字段、缺 `ca_file`、单边 cert/key、非引用口令、非法 `domain_name` 各自被拒且字段名正确；
      合法组合进入 `SourceSelection`。
- [ ] AC2（R3）：不可读 CA、非证书 CA、错配的客户端证书/私钥、不可读口令文件都是启动错误，
      且错误里不含任何 PEM 或口令字节。
- [ ] AC3（R4/R5）：真实 gRPC 上——TLS mock 的 get/watch 成功；需要凭据的 mock 用正确口令连上，
      错误或缺失口令得到 `EtcdError::Auth`；明文客户端连 TLS mock 失败。
- [ ] AC4（R4）：bootstrap 的校验值只存路径与引用；口令在构建 source 时解析；数据面与 LKG 行为不变。
- [ ] AC5（R6）：未引入 `tonic/tls-roots` 或第二个 `rustls-native-certs`；`cargo deny check` 全绿。
- [ ] AC6：`cargo fmt --check`、`cargo clippy --workspace --all-targets --all-features -D warnings`、
      `cargo test --workspace --all-features`、`RUSTDOCFLAGS="-D warnings" cargo doc`、
      feature matrix（默认 / `traffic` / 含 `etcd`）全绿；无 `.rs` 超过 500 行。

## Notes

- 一个交付单元：一份 schema、一条连接路径、一个能证明它的协议替身。不做父/子拆分。
