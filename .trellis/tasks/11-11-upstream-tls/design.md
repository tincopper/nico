# 设计：上游 TLS（rustls 后端）

## 1. 不变量与失败模式

先定不变量，结构由它们推出来。

| 不变量 | 违反时的失败模式 |
|---|---|
| **I1 校验默认开启，不能由文档关掉。** `verify_cert` / `verify_hostname` 永远为真。 | 一个配置笔误把整条路由变成中间人通道，而且日志里看不见。 |
| **I2 信任根只有一个来源：平台库，或一份整体替换它的 bundle。** 不存在"两处各写一半"。 | 同一进程里两个 provider 用不同信任集，排查时无法回答"到底信了什么"。 |
| **I3 握手失败不降级。** 失败必须是 `502 upstream_unreachable`，绝不回退成明文。 | 明文被发往声明为 `https` 的端口，而调用方以为链路是加密的。 |
| **I4 TLS 状态不参与热重载。** CA 在 service 构建时定型，改了要重启。 | reload 后一部分连接用新根、一部分用旧根，且没有 generation 能解释。 |
| **I5 进程只有一套传输栈。** 不为 TLS 引入第二个 HTTP 客户端。 | 两套超时/取消/流语义，重试与熔断的依据变成"看是哪条路"。 |

## 2. 模块边界

```
crates/nico-http/src/upstream/
├── mod.rs            peer(): 组装 HttpPeer（不再做策略判断）
├── resolve.rs        host:port → SocketAddr
└── tls/
    ├── mod.rs        TlsPolicy 的公开面
    ├── policy.rs     纯策略：根来源 + PEM bundle 校验。不含 pingora 类型、不做 I/O
    ├── adapter.rs    policy → pingora ServerConf（今天唯一的消费者）
    └── tests.rs      策略与校验的纯单测
```

### 2.1 `policy.rs` 的契约

```rust
/// 信任根的来源。两个分支互斥，没有"追加"。
pub(crate) enum RootSource {
    /// 平台证书库（rustls-native-certs，provider 自己读 SSL_CERT_FILE / SSL_CERT_DIR）。
    Platform,
    /// 一份完整的信任根集合，整体替换平台库。
    Bundle(PathBuf),
}

pub(crate) struct TlsPolicy { /* 现在只有一个字段，未来是 verify / 版本 / client cert */ }

impl TlsPolicy {
    pub(crate) fn from_bootstrap(ca_file: Option<PathBuf>) -> Self;
    pub(crate) fn root_source(&self) -> RootSource;
}

/// 纯函数：一段 PEM 字节是否是一个可用的信任根集合。
pub(crate) fn validate_bundle(bytes: &[u8]) -> Result<(), BundleError>;
```

- `policy.rs` 里**不出现** `pingora` 标识符。它是未来 `nico-tls` 的抽取单元（§6）。
- `validate_bundle` 只吃字节：读文件是 bootstrap 的事，策略层不碰文件系统，因此可以在单测里
  覆盖"空文件 / 只有私钥 / 多个证书 / 中途一段坏的"这些形状。
- 校验必须与 pingora 走**同一条解析路径**（`rustls_pemfile::read_all` + `RootCertStore::add`，
  见 `connectors/tls/rustls/mod.rs:276-292` 的 `build_connector`）。原因见 §3.1：解析失败在 pingora
  里是 `unwrap`。因此 `nico-http` 直接依赖 `rustls-pemfile` 与 `rustls`（都已在依赖树里），而不是
  自己做一条宽松的文本检查。

### 2.2 `adapter.rs` 的契约

```rust
/// 把策略写进 Pingora 的进程级配置。唯一 import pingora TLS 类型的位置。
pub(crate) fn apply(policy: &TlsPolicy, conf: &mut ServerConf);
```

只做一件事：`Bundle(path)` → `conf.ca_file = Some(path.display().to_string())`；`Platform` → 不动。

**为什么只到 `ServerConf`，不碰 `PeerOptions`**：`verify_cert` / `verify_hostname` 永远是
`true`，而 `PeerOptions` 的默认值就是 `true`（`upstreams/peer.rs:582-588`）。为"设置成默认值"写
一层映射是没有第二个消费者的抽象。等出现第一个真正要改的 peer 级字段（每 provider 的 CA、mTLS），
再把 adapter 扩成 `apply_peer(policy, &mut HttpPeer)`。

### 2.3 `upstream/mod.rs` 的变化

- 删掉 `TLS_BACKEND_ENABLED` 与 `refuse_unavailable_tls`；`peer()` 变成
  `resolve` → `build` 两步。
- `build()` 保持今天的两行（超时 + SNI），不新增 TLS 分支——TLS 事实由 connector 决定，
  peer 只声明"这个目标是 https"。

## 3. 配置契约

### 3.1 bootstrap 新增 `tls` 段

```yaml
tls:                    # 可选；省略 = 用平台证书库
  ca_file: /etc/nico/roots.pem
```

- 段存在但 `ca_file` 缺失 / 为空 → 启动失败（字段名 `tls.ca_file`）。空段等于没说，按错处理。
- 路径不可读、或字节不是有效的信任根集合 → 启动失败，同样指名 `tls.ca_file`。**必须在启动期
  拒绝**：pingora 的 `tls::Connector::new` 把 `build_connector` 的结果直接 `unwrap()`
  （`connectors/tls/rustls/mod.rs:50`），而这条路径在 `HttpProxy::new`（`pingora-proxy/lib.rs:222`）
  里，没有任何可用的错误出口——一份坏 CA 文件的结局是**进程 panic**，不是干净的启动失败。
- 语义是**整体替换**，文档必须写清："这是完整信任根集合，不是追加。要同时信任公网与私有 CA，
  请把两者拼进同一个文件。"

### 3.2 数据流

```
bootstrap.yaml (tls.ca_file)
        │ 启动期：读字节 + validate_bundle
        ▼
HttpSettings { tls: TlsPolicy }            nico-http/settings.rs
        │
        ├─ service::server_conf(settings) ──► tls::adapter::apply(&mut ServerConf)
        │                                              │
        │                                              ▼
        │                                     ConnectorOptions::from_server_conf
        │                                     （pingora-proxy/lib.rs:222，建 service 时执行一次）
        ▼
   request path: Destination{scheme:https, host, port} → HttpPeer(tls=true, sni=host)
```

`resources.yaml` 不加任何 TLS 字段：信任根是进程级事实，不是某个 provider 的属性。

### 3.3 平台信任库也要在启动期探

`serve_with` 在构建 service **之前**调用 `tls::ensure_usable(&policy)`（`tls/mod.rs`，与
`policy.rs` 的纯规则分开）：

- `RootSource::Platform` → `rustls_native_certs::load_native_certs()`（与 pingora 同一个函数，同样尊重
  `SSL_CERT_FILE` / `SSL_CERT_DIR`），要求至少一个证书；
- `RootSource::Bundle(path)` → 重新 `fs::read` + `validate_bundle`。

失败 → 像 metrics 端口预检一样记一条 `error!` 并 `FAILURE_EXIT_CODE` 退出，不进入 `Server::run`。

两条理由，都是实测事实：

1. `pingora-core/src/connectors/tls/rustls/mod.rs:50` 是 `build_connector(...).unwrap()`，而
   `load_native_certs` 在无法访问平台信任库的进程里返回 `errSecIO (-36)`（本机实测：
   `rustls-native-certs 0.7.3` 在非图形登录会话里即如此）。结果是**建 service 时 panic**，不是启动错误。
2. 装配期重新读 bundle 关掉了 bootstrap 解析到 server 构建之间的窗口：connector 打开的是它自己那次
   读取，文件在那之后损坏、被删或被替换，仍然会让它 `unwrap()` 掉。

为什么在装配期而不是 bootstrap 解析期：解析期只负责“文档写得对不对”（带字段名的拒绝），
装配期负责“这个环境能不能真的跑”。放在解析期会让所有 parse 级单测被宿主平台信任库绑架。

代价要说清楚：这条规则让"平台信任库可读"成为进程能启动的前提，即使该部署只用明文上游。
这是有意的：一个无法验证任何 TLS 对端的网关，启动后只能在首个 https 请求上失败，而那个失败点
离真相（宿主信任库不可用）很远。

## 4. 错误映射

| 场景 | 对外 | 日志 |
|---|---|---|
| 握手失败（证书不受信、协议错误） | `502 upstream_unreachable` | `error.category` + 类型化 cause（`TLSHandshakeFailure`） |
| 主机名不匹配 | 同上 | cause 可区分（rustls `CertificateError`） |
| 握手超时 | `504 upstream_timeout` | 同其它超时 |
| CA 文件本身有问题 | 启动失败，不进入服务 | `tls.ca_file` 字段名 |
| 平台信任库不可读 / 为空 | 启动失败，不进入服务 | 消息提示设置 `tls.ca_file` |

实现点只有一处：`proxy/failure.rs` 的超时谓词补 `TLSHandshakeTimedout`（今天只认
`ReadTimedout | WriteTimedout | ConnectTimedout`，`:94`）。其余分类沿用现有 connect 阶段的映射，
不新增对外 code。

## 5. 备选方案与取舍

| 备选 | 为什么不做 |
|---|---|
| **用 reqwest 走出站**（aiway 的做法） | 违反 I5。aiway 的 pingora 根本没编 TLS 后端，LLM 流量绕开数据面走 reqwest，等于一个进程两套 HTTP 栈；nico 的 LLM 路由就在数据面里（canonical 重编码、连接池、熔断、fallback 都挂在这条链上），照搬要把这些全部重做一遍。 |
| **boringssl** | 需要 cmake/ninja 现场编译，构建时间与工具链代价换不到今天的收益；用户已明确不做。 |
| **openssl** | nginx 用的就是它，长期是对的方向；但那时要重做一次选型与门禁，本任务不为它留骨架（不为一个不存在的实现写 feature）。 |
| **给 `nico-http` 加 `tls` feature** | 目录里没有"不带 TLS 的产物"这个需求；多一个构建形状就要多养一条 `--no-default-features` 门禁，而且 `TLS_BACKEND_ENABLED` 那条假分支会永远留着。TLS 是硬依赖。 |
| **per-provider CA（`PeerOptions.ca`）进本期** | 构造 `CaType = [WrappedX509]` 依赖 pingora 的内部类型（`self_referencing(pub_extras)`），会把 pingora 类型推进 `Destination` 这个接缝，从而推进 `nico-llm`。本期只做进程级，扩展条件见 §6。 |
| **CA 文件热加载** | pingora 的连接器在 service 构建时定型；要做热加载只能重建 service，那是 drain 与 generation 语义的另一场设计。违反 I4 是可接受的代价，但要写明。 |

## 6. Reserved seam：`nico-tls`

本期**不建** crate。抽取的触发条件是任意一条：

1. 出现**第二个 TLS 策略消费者**：远端配置源（etcd / HTTPS）、管理面客户端、OTLP 上报、插件出站、
   主动健康检查——任一落地时就抽 `nico-tls`，内容就是 `policy.rs` 的迁移 + 一个 `rustls::ClientConfig`
   的构造（`pingora-rustls` 与 `reqwest 0.13` 都用 rustls 0.23 + ring，可以共享同一套装备）。
2. 出现**每 provider 一份 CA** 的需求：那时策略从进程级变成文档级，`policy.rs` 仍不依赖 pingora，
   `adapter.rs` 扩成 `apply_peer`。
3. `policy.rs` 长到需要按边界再切（届时是 `policy/pem.rs`、`policy/roots.rs`，仍不是 crate）。

`nico-upstream` 不在候选里：未来的其它 HTTPS 出站各有自己的 endpoint 概念（etcd endpoint、collector
URL、webhook 地址），它们不消费数据面的 `Destination`（那是"路由目标 + Host 重写 + 超时预算"的
合成物）。

## 7. 兼容性与回滚

- 文档兼容：`resources.yaml` 不需要任何改动；`scheme: https` 从"必失败"变成"可用"。
- 行为兼容：明文上游路径完全不变（`HttpPeer` 的 `tls=false` 分支不经过任何新代码）。
- 回滚形状：改动集中在 `nico-http/Cargo.toml`、`upstream/{mod,tls}`、`proxy/failure.rs` 的谓词、
  `nico-server` 的 bootstrap `tls` 段、以及文档。回滚 = 恢复常量与九行拒绝函数，删掉 `tls` 段。
  没有数据迁移，没有线上状态。

## 8. 测试策略

| 层 | 内容 |
|---|---|
| 纯单测（`upstream/tls/tests.rs`） | `validate_bundle` 五种形状；`RootSource` 映射；adapter 写进 `ServerConf` 的字段值 |
| 谓词单测（`proxy/failure.rs` 的既有测试） | `TLSHandshakeTimedout` 归入超时 |
| 集成 e2e（`crates/nico-http/tests/tls_upstream.rs`） | 真 TLS 监听器 + 自签 CA 三场景：成功、不受信、主机名不匹配。证书用静态 fixture（`tests/fixtures/tls/`，100 年有效期），不在测试里生成，避免引入证书生成依赖与时钟脆弱性 |
| 公网冒烟（`#[ignore]`） | 连一个公开 https 站点，证明真实根证书库可用。只在本机手动跑并记录结果 |

`nico-http` 的 dev-dependencies 需要 `tokio-rustls`（起 TLS 监听器）；生产依赖不变。

## 9. 被接受的限制

- CA 改了要重启（I4）。
- `tls.ca_file` 是整体替换，不是追加（§3.1）。
- 上游 ALPN 固定 H1；HTTP/2 上游不在本期。
- 每 provider 一份 CA 不可用；多租户私有 CA 需要等 §6 的第二个触发条件。
- 平台信任库不可读时进程拒绝启动（包括纯明文部署），见 §3.3。
