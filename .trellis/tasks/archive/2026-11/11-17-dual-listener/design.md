# 技术设计：bootstrap 配置 nginx 对齐

> 需求见 `prd.md`；设计依据见 `research/nginx-config-analysis.md`（nginx 调研 + 全量映射表）与 `research/option-b-analysis.md`（D1-D4 缺陷分析）。

## 1. 不变量

1. **一段一职责**：`runtime` = 进程旋钮，`http` = 协议层（入站 listener），`proxy` = 出站（上游信任根），`config_source` = 配置获取与更新，`metrics` = 自观测。顶层不再有裸标量；段的存在即该层事实的家；入站身份与出站信任不再共用 `tls` 名。
2. **条目即 listener**：`http.listeners[]` 的每个条目是一个 listener；`tls` 块存在即 TLS listener（nginx `listen 443 ssl` 的 YAML 化）。
3. **启动期失败，不是请求期 panic**：证书在绑定前读取并校验两次（bootstrap + service 复检）。
4. **没有静默降级**：TLS 条目只接受 TLS；明文条目只接受明文；`h2c` 只作用于明文条目。
5. **显式取代隐式**：`http` 段缺席 = 启动拒绝（serde missing field），不是默认 8080。
6. **传输判定来自连接**：`x-forwarded-proto` 由 `ssl_digest` 决定，不依赖进程级标志。

## 2. 配置 schema（Raw 层）

```yaml
runtime:                     # 可选；缺席 = 全默认
  threads: 1
  grace_period_ms: 5000
  graceful_shutdown_ms: 30000

http:                        # 必填（serde required）
  h2c: false
  listeners:
    - listen: 0.0.0.0:8080   # 明文条目
    - listen: 0.0.0.0:8443   # TLS 条目：tls 块存在即 TLS
      tls:
        cert_file: /etc/nico/tls/fullchain.pem
        key_file: /etc/nico/tls/key.pem

proxy:                      # 上游信任根（出站）：nginx proxy_ssl_* 家族，原顶层 tls 改名
  tls:
    ca_file: ...

config_source:              # 配置获取子系统（资源文档从哪来 + 怎么更新）
  type: local
  path: ./config/resources.yaml
  max_bytes: 1048576
  watch: notify+signal
  debounce_ms: 200          # 以下五项原为顶层裸标量，本任务迁入
  max_wait_ms: 1000
  cache_dir: ./.tmp/cache
  allow_bootstrap_cache: false
  schema_bounds:
    max_routes: 1024

metrics:                     # 不变，碰撞对象改为地址集合
  listen: ...
```

### Rust 类型

```rust
// bootstrap.rs —— RawBootstrap
runtime: Option<runtime::RawRuntime>,   // None = 默认
http: http::RawHttp,                    // Option<RawHttp> 改为必填
proxy: Option<proxy::RawProxy>,         // None = 平台信任根；原 tls 字段改名封装
config_source: config_source::RawConfigSource,  // 归组：type/path/watch + debounce/max_wait/cache/schema_bounds 迁入
// 顶层裸标量全部消失：debounce_ms、max_wait_ms、cache_dir、allow_bootstrap_cache、schema_bounds
// metrics 不变
```

// bootstrap/proxy.rs（新文件）
pub(super) struct RawProxy {
    tls: Option<tls::RawTls>,           // 语义原样平移：缺席 = 平台根、空段 = 错误
}

// bootstrap/runtime.rs（新文件）
pub(super) struct RawRuntime {
    threads: Option<usize>,
    grace_period_ms: Option<u64>,
    graceful_shutdown_ms: Option<u64>,
}

// bootstrap/http.rs
pub(super) struct RawHttp {
    h2c: Option<bool>,
    listeners: Vec<RawListener>,        // 非空在 resolve 校验（serde 无法表达 min_items）
}
pub(super) struct RawListener {
    listen: String,
    tls: Option<RawTlsIdentity>,        // 存在即 TLS 条目
}
// RawTlsIdentity = 现 http/tls.rs 的 RawHttpTls 平移重命名
// bootstrap/http/tls.rs → bootstrap/http/listener.rs（条目解析 + 证书校验）
```

`deny_unknown_fields` 保持：`http.tls`、`http.threads` 等旧字段自动成为未知字段拒绝，无需显式删除逻辑。

## 3. 类型模型（nico-http）

```rust
// server_settings.rs（新文件，一个数据结构一个文件）
pub struct ServerSettings {
    threads: usize,
    grace_period_ms: u64,
    graceful_shutdown_ms: u64,
}
impl ServerSettings {
    pub const fn new() -> Self;   // 默认 1 / 5000 / 30000
    pub fn resolve(
        threads: Option<usize>,
        grace_period_ms: Option<u64>,
        graceful_shutdown_ms: Option<u64>,
    ) -> Result<Self, ServerSettingsError>;
    pub fn threads(&self) -> usize;
    pub fn grace_period_ms(&self) -> u64;
    pub fn graceful_shutdown_ms(&self) -> u64;
}

// settings.rs —— 校验后的类型面
pub enum Listener {
    Plain { listen: String },
    Tls { listen: String, tls: InboundTls },
}
pub struct HttpSettings {
    listeners: Vec<Listener>,      // 非空，地址两两不同
    h2c: bool,
    tls: TlsPolicy,                // 上游信任根（数据面事实，不变）
}
impl HttpSettings {
    pub fn resolve(
        listeners: Vec<Listener>,
        h2c: Option<bool>,
        tls: TlsPolicy,
    ) -> Result<Self, HttpSettingsError>;
    pub fn listeners(&self) -> &[Listener];
    pub fn listener_addresses(&self) -> Vec<&str>;   // metrics 碰撞校验用（全部条目）
    pub fn plain_addresses(&self) -> Vec<&str>;      // h2c 的作用域（仅明文条目）
    pub fn h2c(&self) -> bool;
    pub fn tls(&self) -> &TlsPolicy;
}
```

- **错误类型拆分**：`HttpSettingsError` 字段族收窄为 `http.listeners`（空列表、地址重复、地址非法）与 `http.h2c`（无明文条目时为 true）；新增 `ServerSettingsError`（threads/grace/shutdown，现规则平移，消息引用常量）。
- `listen()` / `inbound_tls()` 访问器删除；`Listener` 枚举保证 TLS 条目结构上无法携带 h2c。
- Raw → Settings 转换在 bootstrap 层完成（`RawListener` 单一结构体避免 untagged enum 的劣质报错，校验后转 `Listener` 枚举）。

## 4. service 组装（nico-http）

```rust
pub fn server_conf(runtime: &ServerSettings, tls: &TlsPolicy) -> Arc<ServerConf>;
// grace_period_seconds = whole_seconds(runtime.grace_period_ms())
// graceful_shutdown_timeout_seconds = whole_seconds(runtime.graceful_shutdown_ms())
// threads = runtime.threads()
// tls::apply(tls, &mut conf)      // ca_file，不变

pub fn service(
    conf: &Arc<ServerConf>,
    state: Arc<HttpStateHandle>,
    settings: &HttpSettings,
) -> Result<Service<HttpProxy<HttpApp, ()>>, ServiceBuildError>;
// 遍历 listeners：
//   Plain { listen }            → service.add_tcp(listen)
//   Tls { listen, tls }         → 复检（读文件 + validate_server_identity）
//                                 → StaticCertResolver + enable_h2 → add_tls_with_settings
// options.h2c = settings.h2c()   // service 级；TLS listener 走 ALPN，不受影响
// listeners 非空由 resolve 保证；复检失败 → ServiceBuildError（现路径不变）
```

- `HttpApp::inbound_tls` / `set_inbound_tls` / `inbound_tls()` 删除。
- `inbound.rs` 的 `InboundTls` / `validate_server_identity` / static resolver 全部复用不动。

## 5. 按连接传输判定（proxy/hooks/request.rs）

```rust
// upstream_request_filter：客户端未提供 x-forwarded-proto 时
let is_tls = session
    .digest()
    .and_then(|digest| digest.ssl_digest.as_ref())
    .is_some();
request.insert_header("x-forwarded-proto", if is_tls { "https" } else { "http" })?;
```

可靠性依据（pingora-core 0.9.1 源码核实）：H1 `digest()` 恒 `Some`（`protocols/http/server.rs:849`）；H2 不可变 `digest()` 恒 `Some`（`v2/server.rs:939-941`）；`ssl_digest` 在建连时由底层流写入（`v1/server.rs:170`）。`digest()` 为 `None`（subrequest/custom 路径，本构建未使用）防御性按明文处理。

单测可直接构造 `SslDigest`（字段全 `pub`：cipher/version/organization/serial_number/cert_digest，`tls/digest.rs:23-33`）覆盖两分支；e2e 用真 TLS 连接兜底。

## 6. bootstrap resolve 流程（nico-server）

```rust
// bootstrap.rs resolve()
let runtime = match &self.runtime {
    Some(raw) => raw.resolve()?,
    None => ServerSettings::new(),
};
let tls_policy = match &self.proxy {
    Some(raw) => raw.resolve()?,       // proxy.tls → TlsPolicy，语义原样平移
    None => TlsPolicy::from_bootstrap(None),   // 平台信任根
};
let http = self.http.resolve(tls_policy)?;
// http resolve 内：listeners 非空、地址两两不同、条目地址合法、
//                  TLS 条目读文件 + validate_server_identity、h2c 无明文拒绝
let metrics = match &self.metrics {
    Some(raw) => Some(raw.resolve(http.listener_addresses())?),
    None => None,
};
```

- `BootstrapConfig` 增加 `runtime()` 访问器（`serve_with` 用它调 `server_conf`）。
- reload 旋钮归组：`RawConfigSource` 增加五个字段（全部 Option，缺省沿现默认）；`BootstrapConfig` 内部字段与访问器不变——只有 serde 字段路径变，消费方（reload 管线）零改动。
- `metrics.resolve(&self, listeners: &[&str])`：碰撞对象改为解析后的地址集合（D3 修正）。
- `http` 段缺席 = serde missing field（Parse 错误）；`listeners` 空列表 = `InvalidField("http.listeners")`。不需要新的 `BootstrapError` 变体。
- `proxy` 段语义原样平移：缺席 = 平台信任根；`proxy.tls` 存在但空 = `InvalidField("proxy.tls.ca_file")`（present-but-empty 原则）；`ca_file` 存在 = 替换平台根。`RawTls`/`TlsPolicy`/`ensure_usable` 内部名不变，只有配置面路径变。

## 7. 测试矩阵

**nico-http 单元**

| 断言 | 位置 |
|---|---|
| `ServerSettings` 默认值/边界（threads 1..=256、grace/shutdown 范围、shutdown ≥ grace） | `server_settings.rs` 测试 |
| `HttpSettings` resolve：空 listeners 拒绝、地址两两不同、h2c 无明文拒绝、h2c 与 TLS 条目并存通过 | `settings.rs` 测试 |
| `service()`：Plain → add_tcp、Tls → 复检+resolver+add_tls、h2c 传递 | `service/tests.rs` |
| `server_conf`：threads 透传、秒数向上取整、shutdown ≥ grace 平移 | `service/tests.rs` |
| `x-forwarded-proto`：构造 `SslDigest`（Some → https）/ 无 digest（→ http）；客户端已提供值不覆盖 | `proxy/tests` |

**nico-server 单元**

| 断言 | 位置 |
|---|---|
| `runtime` 段：缺席全默认、各字段越界拒绝、规则平移 | `bootstrap/tests` |
| `proxy` 段：缺席 = 平台根、空段错误、`ca_file` 替换语义、字段路径 `proxy.tls.ca_file`；旧顶层 `tls` 被拒 | `bootstrap/tests` |
| `config_source` 归组：新路径字段生效（debounce/cache/bounds）、旧顶层裸字段被拒、语义不变 | `bootstrap/tests` |
| `http.listeners`：明文/TLS 条目、空列表拒绝、地址重复拒绝、`http.tls` 旧字段被拒 | `bootstrap/tests/http_tls.rs`（重写） |
| TLS 条目证书坏路径（缺字段/不可读/坏 PEM/不匹配）→ 字段 `http.listeners[N].tls.*` | 同上 |
| metrics 碰撞：与任一 listener 地址相同拒绝 | `bootstrap/tests` |

**e2e（tests/tls_inbound.rs 改造 + 新增）**

| 场景 | 断言 |
|---|---|
| TLS-only（1 个 TLS 条目） | 真 TLS 客户端握手 + 200 + 上游见 `https`；明文连此口无响应；ALPN h2 协商 |
| 双 listener | 明文口 200 + 上游见 `http`；TLS 口 200 + 上游见 `https`；交叉连接无 HTTP 响应 |
| 纯明文回归 | 既有明文 e2e 零改动通过 |

**诚实标注**：TLS 相关既有测试**改写而非零改动**（schema 路径 `http.tls.*` → `http.listeners[N].tls.*`），行为断言平移；明文相关测试零改动。

## 8. 影响面与文件预算

| 文件 | 改动 |
|---|---|
| `nico-http/server_settings.rs` | 新增（~150 行） |
| `nico-http/settings.rs` | 重造：Listener 枚举、resolve、错误族收窄 |
| `nico-http/service.rs` | server_conf 签名、listeners 遍历注册 |
| `nico-http/proxy.rs`、`hooks/request.rs` | 删标志、按连接判定 |
| `nico-http/inbound.rs` | 不动 |
| `nico-server/bootstrap.rs` | runtime 段、http required、proxy 段接线、config_source 归组、resolve 流程 |
| `nico-server/bootstrap/runtime.rs` | 新增 |
| `nico-server/bootstrap/proxy.rs` | 新增：RawProxy 封装（tls 子段，空段处理） |
| `nico-server/bootstrap/tls.rs` | 字段路径改 `proxy.tls.ca_file`，其余语义不动 |
| `nico-server/bootstrap/source.rs` | RawConfigSource 归组五个 reload 旋钮字段（Raw 层搬家，BootstrapConfig 内部不变） |
| `nico-server/bootstrap/http.rs` | RawHttp 重造（h2c + listeners） |
| `nico-server/bootstrap/http/listener.rs` | 原 http/tls.rs 平移重命名（条目证书校验） |
| `nico-server/bootstrap/metrics.rs` | 碰撞对象改地址集合 |
| `nico-server/server.rs` | serve_with 适配（bootstrap.runtime()） |
| tests | `http_tls.rs` 重写、`tls_inbound.rs` 改造+新增、`support/http.rs` 更新 |
| 文档 | `config/bootstrap.yaml`、`README`、spec 三件 |

无新依赖；`tokio-rustls` 仍 dev-only。

## 9. 风险与取舍

| 风险 | 处置 |
|---|---|
| 昨天刚交付的代码大面积重构 | pre-1.0 窗口，行为断言平移、分阶段 checkpoint（见 implement.md）；rollback 点按 crate 划分 |
| Pingora `HttpServerOptions.h2c` 是 service 级 | `http.h2c` 段级与之 1:1 映射；TLS listener 走 ALPN 不受影响 |
| serde missing field 报错不够友好 | 接受：schema 错误透传；`config/bootstrap.yaml` 示例保证常用形态正确 |
| `SslDigest` 单测构造 | 字段全 pub，直接构造；e2e 真连接兜底 |
| metrics 签名变化波及 | resolve 输入改为地址切片，调用点唯一（bootstrap.rs） |

## 10. 交付边界

- 本任务不碰 `resources.yaml`（nginx 对齐审查是后续任务），不引入 nginx 语法兼容。
- 上游 TLS（`11-11-upstream-tls`）、指标 listener TLS、证书热加载不动。
