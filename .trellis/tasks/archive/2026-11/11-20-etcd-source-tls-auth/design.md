# 设计：etcd 配置源 TLS 与认证

## 1. 不变量与失败模式

1. **配置源是进程的基础设施，不是文档。** 它的信任根、客户端身份与口令在启动期定型；运行期没有
   第二条来源能改它，reload 只换它读到的文档。
2. **传输方式由运维一次说清。** `https` 端点与 `tls` 块必须同时出现；缺一半是启动错误，不是
   "按端点猜 scheme"。猜错的方向要么明文传输配置文件，要么连不上。
3. **信任根只来自明确的文件。** etcd 连接不使用平台证书库。理由有两条：把信任根写成启动事实，
   与 `proxy.tls.ca_file` 的"显式包"语义一致；不引入 `tonic/tls-roots`，它会把
   `rustls-native-certs 0.8` 与工作区 pin 的 `0.7.3` 一起装进二进制。
4. **口令是引用，不是值。** `password_ref` 只接受 `env:` / `file:`；校验后的 bootstrap 只保存引用，
   值在构建 source 时读一次。
5. **bootstrap 只存路径。** CA 与客户端身份的字节不进入 `BootstrapConfig`；校验期读一次，
   构建 source 时再读一次（与入站 TLS 同一理由：两次读取之间文件被换掉，必须是可读的启动错误）。

失败模式与对应行为：

| 失败 | 行为 |
|---|---|
| `https://` 端点但无 `tls` 块 | 启动失败，字段 `config_source.endpoints[i]` |
| `tls` 块 + `http://` 或裸 `host:port` 端点 | 启动失败，字段 `config_source.endpoints[i]` |
| `tls` 块缺 `ca_file` | 启动失败，`config_source.tls.ca_file` |
| CA 不可读 / 不是证书集合 | 启动失败，`config_source.tls.ca_file` |
| 只有 `cert_file` 或只有 `key_file` | 启动失败，`config_source.tls.key_file`（成对规则） |
| 客户端证书/私钥不可读或不配对 | 启动失败，`config_source.tls.cert_file` / `.key_file` |
| `auth` 缺 `user`/`password_ref`、引用非法 | 启动失败，`config_source.auth.user` / `.password_ref` |
| 口令文件或环境变量不可读 | 构建 source 失败：`config_source.auth.password_ref` |
| 口令超过大小上限（`env:` 或 `file:`） | 构建 source 失败：`config_source.auth.password_ref` |
| TLS 材料文件超过大小上限（校验期或构建期） | 启动失败，`config_source.tls.*` |
| 构建 source 时 CA/身份被换掉 | 构建 source 失败：`config_source.tls.*` |
| 端点拒绝凭据 | `EtcdError::Auth`，启动失败（没有降级形态） |
| 端点不可达 / TLS 握手失败 | `EtcdError::Connect`，启动失败 |

## 2. 契约

### 2.1 schema

```yaml
config_source:
  type: etcd
  endpoints: [https://etcd-0:2379, https://etcd-1:2379]
  key: /nico/resources.yaml
  max_bytes: 1048576
  tls:
    ca_file: /etc/nico/etcd-ca.pem          # required with the block; replaces any platform store
    cert_file: /etc/nico/etcd-client.pem    # optional, with key_file
    key_file: /etc/nico/etcd-client-key.pem
    domain_name: etcd.internal              # optional; verified name for every endpoint
  auth:
    user: nico
    password_ref: env:ETCD_PASSWORD
```

字段路径与错误名一致：`config_source.tls.{ca_file,cert_file,key_file,domain_name}`、
`config_source.auth.{user,password_ref}`。两个块都是 `deny_unknown_fields`；写进 `local` 是未知字段。

### 2.2 端点 × 传输矩阵

| endpoints | `tls` 块 | 结果 |
|---|---|---|
| 全 `https://` | 有 | 接受，TLS 连接 |
| 全 `https://` | 无 | 拒绝（`endpoints[i]`：需要 `config_source.tls`） |
| `http://` 或裸 `host:port` | 无 | 接受（现状） |
| 混合 scheme | 任意 | 拒绝（`endpoints`：必须统一） |
| `http://` 或裸 `host:port` | 有 | 拒绝（`endpoints[i]`：写明 `https://`） |

混合 scheme 的规则是新增的：etcd-client 把端点列表交给一个 channel 池，TLS 配置对整个客户端生效，
所以"一个 https、一个 http"只能有一种实际传输，说清楚比猜好。

平地对（`http://` 或裸 `host:port`）配 `tls` 被拒，是本项目的策略而不是客户端的能力：
`etcd-client` 对裸地址会自己前缀 `https://`，也会因为 `http://` + TLS 报 `InvalidArgs`。
要求写明 `https://` 的理由是：一份写了 `tls` 的 bootstrap 不该靠客户端去推断端点说的是哪种传输。

### 2.3 `domain_name`

可选。给出时对所有端点使用同一个证书校验名；省略时用端点主机名（客户端默认）。多端点跨不同域名
的证书需要各自的 `domain_name`，本版不支持——一个客户端只校验一个名字，这是 etcd-client 的形状，
不是本项目的限制。文档写明。

## 3. 结构

### 3.1 `nico-server` 的 bootstrap 拆分

`bootstrap/source.rs` 现在 482 行，加 etcd 的 TLS/认证解析必超 500。按既有边界拆：

```
crates/nico-server/src/bootstrap/
  source.rs          # 入口：SourceSelection/SourceSection、RELOAD_KEYS、类型分发、reload 旋钮
  source/etcd.rs     # etcd body：端点形状、scheme × tls 规则、tls/auth 块到 SourceSelection
  source/bounds.rs   # RawSchemaBounds + 算子上限（既有代码原样移动）
```

`source/etcd.rs` 是"一个源类型"的边界，`source/bounds.rs` 是"本文档形状"的边界；两者都不与入口
共享可变状态，移动是纯搬迁 + 新增。

`SourceSelection::Etcd` 从 `{endpoints, key}` 变为 `{endpoints, key, tls: Option<EtcdTls>, auth: Option<EtcdAuth>}`：

```rust
pub(super) struct EtcdTls {                       // paths only, never bytes
    pub(super) ca_file: PathBuf,
    pub(super) identity: Option<(PathBuf, PathBuf)>,   // cert, key
    pub(super) domain_name: Option<String>,
}
pub(super) struct EtcdAuth {
    pub(super) user: String,
    pub(super) password_ref: String,              // env:/file:, resolved at source build
}
```

bootstrap 校验期读取 CA 与（可选）客户端身份：CA 用 `nico_http::validate_bundle`；身份对用
`nico_http::validate_server_identity`。后者名字带 server，但它校验的事实是"一条证书链 + 一把私钥
能组成一个签名身份"——方向由调用者决定，函数本身不读方向。这一点写在调用点，不新增一个同义 API。

### 3.2 `nico-config-etcd` 的连接选项

```rust
pub struct EtcdTlsOptions {
    ca_pem: Vec<u8>,
    identity: Option<(Vec<u8>, Vec<u8>)>,   // cert chain, private key
    domain_name: Option<String>,
}
pub struct EtcdCredentials { user: String, password: String }
pub struct EtcdConnectOptions {
    tls: Option<EtcdTlsOptions>,
    credentials: Option<EtcdCredentials>,
}
impl Default for EtcdConnectOptions { /* both None: today's plaintext client */ }

impl GrpcEtcdClient {
    pub async fn connect(endpoints: &[String], options: EtcdConnectOptions)
        -> Result<Self, EtcdError>;
}
```

- 选项以**字节**接收 TLS 材料：这个 crate 不认识 bootstrap 文件，读文件是调用者的事；它只负责把
  字节变成 `ClientTlsConfig`（`Certificate::from_pem`、`Identity::from_pem`、`domain_name`）。
- `connect` 的内部：`etcd_client::ConnectOptions::new()` + `.with_tls(..)`（有 TLS 时）+
  `.with_user(..)`（有凭据时），失败仍走既有 `classify`：TLS 握手与不可达是 `Connect`，
  `Unauthenticated`/`PermissionDenied` 是 `Auth`。
- 没有凭据时**不**调用 `with_user`：etcd 的匿名访问与"空用户名密码"不是一回事。

`nico-config-etcd` 的 `client` feature 同时打开 `etcd-client/tls`：

```toml
client = ["dep:etcd-client", "dep:tonic", "etcd-client/tls"]
```

不加新 feature：这一整条 tonic 栈只在 `client` 打开时存在，而 `client` 从产品二进制里被
`nico-server` 的 `etcd` feature（默认关）挡住。启用 `txn`/`tls-roots` 都不需要。

### 3.3 装配：`source_build.rs`

`build_source` 的 etcd 分支多两件事：

1. 解析口令：`password_ref` 走 `env:`（`std::env::var`）或 `file:`（逐字节读，保留换行——
   与资源配置的 `secret_ref` 同一约定）；失败 → `SourceBuildError::Setting { field: "config_source.auth.password_ref" }`。
   两种来源以同一上限（8 KiB）封顶：`file:` 在读取处生效，`env:` 在两条来源共同的收口处生效——
   环境变量的值没有文件读取兜底，误指向的引用不能把任意大的值载入进程。
2. 读 TLS 材料并构造 `EtcdConnectOptions`：CA 与客户端身份再读一次；读取或配对失败 →
   `SourceBuildError::Setting { field: "config_source.tls.<file>" }`。每次读取都以 1 MiB 为上限
   （校验期与构建期共用一个常量），且上限由读取本身执行（`nico_config::read_bounded`）：误指向
   的路径不能把任意文件载入进程，校验期放行的文件也不会在构建期因大小被拒。

`SourceBuildError` 新增一个变体承载"bootstrap 字段在构建期不可用"：

```rust
#[error("bootstrap field `{field}` is invalid: {reason}")]
Setting { field: Cow<'static, str>, reason: Cow<'static, str> },
```

`category()` 为 `Client`：这是运维写错文件，不是上游故障，与 `BootstrapError::InvalidField` 同一类。
口令与 PEM 字节不出现在 `Display` 里。

### 3.4 协议替身

`MockServer` 加一个 builder，保留 `start()` 作为无参快捷方式：

```rust
MockServer::builder()
    .identity(cert_pem, key_pem)          // optional: serve TLS
    .credentials(user, password)          // optional: require Basic auth metadata
    .start()
```

- TLS 走 `tonic::transport::ServerTlsConfig::new().identity(Identity::from_pem(..))`；fixture 由调用者
  以字节传入，crate 不认识测试目录。
- 鉴权按 etcd 的真实形状实现：客户端在连接时把用户名/口令交给 `Authenticate`，拿回 token，之后每个
  请求带这个 token。所以 mock 分两半：`Authenticate` 只在凭据匹配时签发 token（否则
  `unauthenticated`），KV/Watch 前面的 interceptor 只接受这个 token（否则 `unauthenticated`）。
  token 由凭据对派生，只能凭正确口令兑换，不携带口令本身；这是辅助设施，不需要抗猜。
  只检查 `authorization: Basic ...` 的实现是错的：`etcd-client` 的 `Authenticate` 调用不带这个头，
  正确口令也会被拒。
- 明文客户端连 TLS mock：tonic 的 TLS 服务端在握手上关连接，测试断言 `Connect`，不是明文成功。

## 4. 测试策略

| 层 | 覆盖 | 位置 |
|---|---|---|
| bootstrap 解析 | 端点 × tls 矩阵；缺/坏 CA；单边身份；口令引用规则；未知字段 | `bootstrap/tests/etcd_source.rs` |
| 启动期读取 | 不可读 CA、错配身份、不可读口令文件的字段名与无泄漏 | 同上（临时目录） |
| 真实 gRPC | TLS mock 上 get/watch；需要凭据的 mock（正确/错误/缺失） | `crates/nico-config-etcd/src/tests.rs` 或 `tests/` |
| 端到端 | 完整 bootstrap → source 构建 → 通过 TLS mock 读到文档 | `crates/nico-server/tests/etcd_bootstrap.rs` 变体 |
| 不回归 | 明文 etcd 路径、local 源、数据面 | 既有测试不改断言 |

工作区根 `fixtures/tls/` 的 fixture（`localhost.pem` / `localhost.key` / `test-root.pem`，
`nico-http`、`nico-config-etcd` 与 `nico-server` 的测试共用，约定见其 README）可直接用作
"etcd 服务端证书 + 客户端信任根"：客户端用 `test-root.pem` 作 CA，端点写 `https://localhost:<port>`。

## 5. 兼容、发布与回滚

- 不带 `tls`/`auth`：全部行为不变；`EtcdConnectOptions::default()` 就是今天的三参数 `connect`。
- 带 `tls`：新字段，旧二进制以未知字段拒绝启动 —— 期望方向（不会静默明文连接）。
- 依赖：`etcd-client/tls` 已在 `client` feature 内；tonic/rustls/tokio-rustls 均已在依赖图中，
  `Cargo.lock` 只多依赖边，不新增 crate 版本（AC5 用 `cargo tree` 与 `cargo deny check` 验证）。
- 回滚：删掉两个块并重启；无数据迁移。

## 6. 被否决的替代方案

| 方案 | 否决理由 |
|---|---|
| 平铺 `config_source.ca_file` / `cert_file` / `key_file` | 与"`tls` 块出现即 TLS"的一致性破功；也无法表达"写了 cert 忘了 key"这个具体错误 |
| 平台信任库回退（`tonic/tls-roots`） | 引入第二份 `rustls-native-certs`（0.8 与 pin 的 0.7.3），且让 etcd 的信任根变成环境事实而不是配置事实 |
| 口令明文写在 bootstrap | 与仓库既有 `secret_ref` 约定冲突；bootstrap 会随镜像分发 |
| 只在 connect 处校验客户端身份 | 错误会变成"连不上"而不是"`config_source.tls.cert_file` 与 `key_file` 不配对" |
| 单独的 `etcd-tls` cargo feature | 没有要挡住的目标：整个 client 已由 `nico-server` 的 `etcd` feature 挡在产品二进制之外 |
| 端点各自给 `domain_name` | 一个客户端只校验一个名字；多名字需要多个 source，属另一个能力 |
