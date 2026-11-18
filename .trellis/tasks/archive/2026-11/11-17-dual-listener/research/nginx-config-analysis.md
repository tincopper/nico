# nginx 配置设计调研 → nico bootstrap 形态决策

> 触发：用户对 `http.tls.listen` 形态不满，倾向职责分层（协议层 vs 服务应用层），要求参考 nginx 的配置设计思路再定形态。本文先盘 nginx 的机制，再映射到 nico 的事实，给出推荐形态。不改代码。

## 1. nginx 的配置机制盘点

nginx.conf 的组织单位是**上下文（context）**，层次固定：`main → events → http → server → location`。作用域即语义——一条指令属于哪个上下文，就描述哪个层面的事实。

| 机制 | nginx 形态 | 语义 |
|---|---|---|
| **main 上下文** | `worker_processes 4; user www; error_log ...;` 裸指令，不包段 | **进程级**事实：worker 数、用户、日志位置。协议无关 |
| **http 上下文** | `http { ... }` | **协议层**：HTTP 的全局默认（mime、默认 ssl_protocols、日志格式） |
| **server 块** | `http { server { listen 443 ssl; server_name a.com; ... } }` | **虚拟主机** = 一组 listen 地址 + server_name + locations。一个 server 块可以有多条 listen（同一个 vhost 同时服务 80 和 443） |
| **listen 指令** | `listen 0.0.0.0:443 ssl http2;` | **传输是 listen 的参数**：`ssl`（TLS）、`http2`（h2c/h2）、`default_server` 都是地址旁的旗标 |
| **证书位置** | `ssl_certificate` / `ssl_certificate_key` 写在 server 块 | 身份属于虚拟主机（listener 的身份事实），不在全局 |
| **继承** | http 级指令是所有 server 的默认，server 级可覆盖 | 跨 listener 共享默认值（如 ssl_protocols 统一收紧） |
| **default_server** | `listen 80 default_server;` | 多个 vhost 共享一个地址时，未匹配 server_name 的请求归属 |

三条结构性洞察：

1. **进程事实与协议事实分层**：`worker_processes` 从不进 `http{}`——nginx 从第一天就把"服务应用层旋钮"和"协议层配置"放在不同上下文。
2. **listener 是一等公民，传输是它的参数**：没有 `https_server` 和 `http_server` 两种块；只有一个 `listen` 指令，`ssl` 是它旁边的旗标，证书跟着 listener 走。**TLS 不是一种 listener 类型，是 listener 的一个属性。**
3. **继承是有成本的机制**：它为"几十个 server 共享默认"而生；继承链让配置的生效位置变得不显然（nginx 文档专门有一章讲 directive inheritance 的坑）。

## 2. 映射到 nico 的事实

| nico 的事实 | nginx 对应物 | 借不借 |
|---|---|---|
| 进程旋钮 `threads` / `grace_period_ms` / `graceful_shutdown_ms` | main 上下文的 `worker_processes` | **借**：顶层 `server:` 段（B-a 的 nginx 论据——nginx 从不把 worker 数放进协议层） |
| 数据面 1..N 个 listener（明文/TLS，每 TLS listener 一份证书） | server 块的 listen 指令集合 | **借，摊平**：nico 没有 vhost，server 块塌缩成"listener 条目"列表 |
| `h2c`（明文 h2 开关） | listen 指令上的 `http2` 旗标 | **半借**：语义上属于 listener，但 Pingora `HttpServerOptions.h2c` 是 service 级事实 → nico 放 `http` 段级，作用域=全部明文 listener |
| TLS 证书/私钥跟 listener 走 | `ssl_certificate` 在 server 块 | **借**：`tls: {cert_file, key_file}` 作为 listener 条目的可选块 |
| 无 vhost / server_name 路由 | server 块的核心概念 | **不借**：nico 单应用、按 path 路由，虚拟主机概念不存在，server 块塌缩为条目 |
| 跨 listener 默认值 | http→server 继承 | **不借**：nico 配置面小、显式声明 + `deny_unknown_fields` 与继承相克；继承是"几十个 server"的机制，nico 只有 1..2 个 |
| default_server | 地址共享时的路由归属 | **不借**：无 vhost 即无此问题 |

## 3. 推荐形态（nginx 思路的 nico 翻译）

```yaml
# —— 服务应用层（进程事实，nginx main 上下文的对应物）——
server:                     # 可选；缺省 = 现默认值
  threads: 1
  grace_period_ms: 5000
  graceful_shutdown_ms: 30000

# —— 协议层（nginx http 上下文的对应物）——
http:                       # 必填：数据面段的缺席 = 无 listener = 拒绝启动
  h2c: false                # 作用于全部明文 listener；无明文 listener 时拒绝
  listeners:                # nginx server 块的摊平：一个条目 = 一个 listener
    - listen: 0.0.0.0:8080  # 明文条目
    - listen: 0.0.0.0:8443  # TLS 条目：tls 块存在即 TLS listener
      tls:
        cert_file: /etc/nico/tls/fullchain.pem
        key_file: /etc/nico/tls/key.pem

# —— 上游信任根（不变）——
tls:
  ca_file: ...

metrics:
  listen: ...
```

三种部署形态全部由同一 schema 表达，无模式翻转：

| 部署 | 写法 |
|---|---|
| 纯明文 | `http.listeners` = 1 个明文条目 |
| TLS-only | `http.listeners` = 1 个 TLS 条目（无明文口） |
| 双 listener | 2 个条目 |
| 明文多口 | N 个明文条目（h2c 统一管；未来态，不新增机制） |

### 设计要点

- **条目即 listener**：`listen` 必填；`tls` 可选块**存在即 TLS listener**（nginx `listen 443 ssl` 的 YAML 化）。明文条目写 `tls:` = 错误。
- **h2c 是段级不是条目级**：与 nginx 有意分歧——Pingora 的 `HttpServerOptions.h2c` 是 service 级事实，per-listener 旗标会引入"多个明文 listener h2c 必须一致"的校验规则，换不来真实运维需求。TLS listener 不受 `h2c` 影响（其 HTTP/2 走 ALPN）。
- **`h2c: true` 且无明文 listener** → 拒绝（旋钮命名的对象不存在，fail-closed）。
- **地址两两不同**（解析后比较）——对 `listeners[]` 两两校验，metrics 碰撞检查改为对解析后的地址集合。
- **证书校验路径完全复用**（`validate_server_identity` → `CertifiedKey` → static resolver），字段路径变为 `http.listeners[N].tls.cert_file` / `.key_file`。
- **`http` 段必填**（今天缺席 = 默认 8080 明文；B 形态下缺席 = 拒绝——显式声明取代隐式默认，与"空段是错误"同方向）。

### 类型模型

```rust
// settings.rs —— 校验后的类型面
pub enum Listener {
    Plain { listen: String },
    Tls { listen: String, tls: InboundTls },
}
pub struct HttpSettings {
    listeners: Vec<Listener>,   // 1..=N，两两地址不同
    h2c: bool,                  // 明文 listener 的 h2 开关
    tls: TlsPolicy,             // 上游信任根（数据面事实，不变）
}
// server_settings.rs —— 新文件，一个数据结构一个文件
pub struct ServerSettings { threads, grace_period, graceful_shutdown }
// service.rs
pub fn server_conf(runtime: &ServerSettings, tls: &TlsPolicy) -> Arc<ServerConf>;
pub fn service(conf, state, settings: &HttpSettings) -> Result<...>;
// 注册：遍历 listeners，Plain → add_tcp，Tls → 复检+static resolver+add_tls_with_settings
// x-forwarded-proto：按连接 ssl_digest 判定（决策不变）
```

- Raw 层用单一结构体 `RawListener { listen, tls: Option<RawTlsIdentity> }`（避免 untagged enum 的劣质报错），校验后转 `Listener` 枚举——"TLS 条目不许出现 h2c"这类规则在校验层一次性表达。
- `BootstrapError` 需要表达"无 listener"这类全局性拒绝：新增变体（如 `NoListener`），不复用 `InvalidField`（它没有具体字段）。

## 4. 校验矩阵（最终）

| 配置 | 结果 |
|---|---|
| `http` 段缺席 | 启动拒绝（schema required） |
| `http.listeners` 空列表 | 启动拒绝 |
| 条目地址两两相同 | 启动拒绝，字段指向后一个条目 |
| 条目 `listen` 非法/端口 0 | 启动拒绝（现规则） |
| TLS 条目缺 `cert_file`/`key_file`、文件坏、不匹配 | 启动拒绝，字段 `http.listeners[N].tls.*`（现规则平移） |
| `h2c: true` 且无明文条目 | 启动拒绝，字段 `http.h2c` |
| `h2c: true` + TLS 条目并存 | 允许（不同 listener） |
| `server` 缺席 | 全默认值（5000/30000/1） |
| `server.threads` 越界、`graceful_shutdown < grace_period` | 启动拒绝（现规则平移到 `server.*`） |
| metrics 与任一 listener 地址相同 | 启动拒绝（现规则，校验对象改为地址集合） |

## 5. 对比：本形态 vs 此前两个方案

| | A（`http.tls.listen`） | B（`http`/`https` 双段） | **本形态（listeners 列表）** |
|---|---|---|---|
| listener 表达 | 子段内嵌地址，语义翻转 | 两个段名两种类型 | 条目列表，一段一协议 |
| 传输规则 | h2c+TLS 冲突校验 | 同左（仅接管模式） | 冲突不可表达；仅"无明文却开 h2c"一条 |
| 扩展到多口 | 无处安放 | 需第三个段 | schema 天然容纳 |
| 旋钮归属 | 未解决 | `server` 段 | `server` 段（nginx main 上下文论据） |
| 概念数 | 接管/独立双模式 | 明文段+TLS 段 | 一个概念（listener）× 两种传输 |

## 6. 影响面（相对 option-b-analysis 的增量）

- 前一分析中"顶层 `https` 段"改为"`http.listeners[]` 条目"；`bootstrap/https.rs` → `bootstrap/http.rs` 内 listeners 解析 + `bootstrap/http/listener.rs`（条目解析 + 证书校验，一个边界一个文件）。
- `RawHttp` 变为 `{ h2c, listeners }`；`listen` 顶层字段消失（并入条目）。
- `BootstrapError` 增加全局性变体（无 listener）。
- 其余（ServerSettings 拆出、metrics 集合校验、tests 重写、spec 三件、config 示例、README）与 option-b-analysis 第 4 节一致。

## 7. 未借用的 nginx 机制（显式记录，防回头重造）

- **继承**：nico listener 数量级为 1..2，显式声明足够；继承会让 `deny_unknown_fields` 的封闭性失效。
- **server_name / vhost / default_server**：无虚拟主机路由，不存在地址共享的归属问题。
- **events{}（worker_connections）**：连接级调优未暴露，Pingora 内部管。

## 8. 整体配置的 nginx 对齐分析（迁移视角）

> 用户追加需求：整体配置向 nginx 对齐，让 nginx → nico 迁移方便。先立原则，再给全量映射，最后收敛范围。

### 8.1 “对齐”的定义

- 对齐的是**结构与概念**（上下文→段、指令→字段、心智模型），不是语法：nico 是 YAML + `deny_unknown_fields`，nginx 是 DSL + 继承 + `include`。语法兼容是明确非目标——实现 nginx 指令表 + 继承语义的成本巨大，且锁死演进。
- 名字只在**语义 genuinely 相同**时抄；底层模型不同就保留 nico 名 + 文档映射。抄一个含义不同的名字比不同名更糟——它制造错误预期。
- 迁移便利的真正载体是**映射表**（nginx 指令 → nico 字段），心智迁移 + 查表，不是文件可直接 pipe。

### 8.2 全量映射表

**main 上下文 → `runtime` 段（本任务）**

| nginx | nico | 状态 |
|---|---|---|
| `worker_processes 4;` | `runtime.threads` | 结构对齐；名字不抄——nginx fork 多进程，nico 是 tokio 工作线程，模型不同 |
| `worker_shutdown_timeout 10s;` | `runtime.grace_period_ms` + `graceful_shutdown_ms` | 概念对齐；nico 两阶段（请求宽限 + 总排水上限）比 nginx 单值细 |
| `error_log` / `user` / `pid` / `daemon` | 无 | 部署模型不同：容器 + supervisor 接管，日志走 tracing 规范 |

**events 上下文**：`worker_connections` → 不暴露（Pingora 内部），无对齐需求。

**http 上下文 → `http` 段（本任务）**

| nginx | nico | 状态 |
|---|---|---|
| `http { ... }` | `http:` 段 | ✓ 本任务对齐 |
| `listen 80;` | `http.listeners[].listen` | ✓ 本任务 |
| `listen 443 ssl;` | 条目 `tls:` 块（存在即 TLS） | ✓ 本任务 |
| `listen ... http2;` | `http.h2c`（段级） | 有意分歧：Pingora `HttpServerOptions` 是 service 级 |
| `ssl_certificate` / `ssl_certificate_key` | 条目 `tls.cert_file` / `key_file` | ✓ 本任务：身份跟 listener 走 |
| `ssl_protocols TLSv1.2 TLSv1.3;` | 内置 rustls 默认，不可配 | nico 更严：写死 1.2/1.3，无 1.0/1.1 |
| `access_log` / `log_format` | 结构化请求记录（`record.rs`） | 不同模型：结构化字段 vs 格式串 |
| `keepalive` / `sendfile` / `gzip` | 不暴露 | 静态服务能力不在范围 |

**server 上下文 → `listeners[]` 条目（摊平）**

| nginx | nico | 状态 |
|---|---|---|
| `server { listen ...; }` | `http.listeners[]` 条目 | ✓ 本任务：无 vhost，server 块塌缩为条目 |
| `server_name` / 虚拟主机 | 无 | 不适用：nico 按 path 路由，无 host 路由 |
| `location /x { proxy_pass ...; }` | `routes[]`（resources.yaml） | 概念对齐 → **独立任务**（不动架构，做字段映射与命名审查） |
| `proxy_set_header` | 内置 attribution 策略 + 插件链 | 部分对齐 |

**上游方向（出站）TLS → `proxy` 段（本任务修正）**

| nginx | nico | 状态 |
|---|---|---|
| `proxy_ssl_trusted_certificate /path;` | `proxy.tls.ca_file` | ✓ 本任务：顶层裸 `tls` 改名 `proxy.tls`——nginx 从无裸 `tls` 上下文，出站 TLS 指令全部带 `proxy_` 前缀；且新设计下 `http.listeners[].tls`（入站身份）与顶层 `tls`（出站信任）同名不同义，必须拆名 |
| `proxy_ssl_verify` / `proxy_ssl_protocols` | 内置（验证恒开；协议写死 1.2/1.3） | nico 更严，不可配 |

语义保持不变：`proxy` 缺席 = 平台信任根；`proxy.tls` 存在但空 = 错误（既有原则）；`ca_file` 存在 = 替换平台根（replace-not-append）。

**upstream 上下文 → `upstreams[]`（resources.yaml，已对齐）**

| nginx | nico | 状态 |
|---|---|---|
| `upstream backend { ... }` | `upstreams[]` | 概念已对齐 |
| `server addr weight=3;` | `endpoints[]` + `SmoothWeighted` | ✓ **命名已对齐**：`picker.rs:24` 明言 SmoothWeighted 即 nginx smooth weighted round robin |
| `least_conn;` / `hash $ip;` | `LeastConn` / `IpHash` | ✓ 策略名直接沿用 nginx 词汇 |
| `max_fails` / `fail_timeout` | health（streak / cooldown / 单探测） | 概念对齐 |

**模块概念 → plugins（概念已对齐）**

| nginx | nico | 状态 |
|---|---|---|
| `limit_req` / `limit_conn` | `plugins: rate-limit` | 概念对齐 |
| `auth_request` / `auth_basic` | `plugins: jwt` / `api-key` | 概念对齐 |

**无 nginx 对应（nico 独有，映射表标“无”）**

| nginx | nico | 说明 |
|---|---|---|
| `SIGHUP` reload + `include` | `config_source`（etcd/local）watch + LKG + generation 发布；本任务将 reload 旋钮（debounce/max_wait/cache/bounds）从顶层裸标量归组进该段 | 模型不同：配置中心 vs 静态文件；同样“不重启换配置”，机制不同 |
| — | `providers` / LLM 协议转换 | nico 的差异化能力，无 nginx 概念 |
| — | `metrics` 独立 listener | nginx 走 `stub_status` location |

### 8.3 既有佐证：LB 策命名已在向 nginx 对齐

`nico-lb` 的策略词汇（`RoundRobin`、`SmoothWeighted`、`LeastConn`、`IpHash`）直接取自 nginx，源码注释明言。整体对齐不是新方向，是把已确立的惯例延伸到 bootstrap 层——本任务的 listener/旋钮设计正是缺的最后一块。

### 8.4 命名修正：进程旋钮段不叫 `server`

此前推荐 `server:` 段。以迁移视角复审，这是**主动冲突**：nginx 的 `server{}` 是最核心的协议层概念（虚拟主机），迁移用户看到 `server: { threads }` 会预期里面是 vhost 定义。

修正选项：

| 候选 | 评价 |
|---|---|
| **`runtime:`（推荐）** | 中性、无冲突，直指“服务应用层”；对 nginx 用户不产生误导 |
| `main:` | nginx main 上下文同名，迁移者一眼认出；对非 nginx 用户需解释 |
| 裸顶层键 | 最忠实 nginx main（裸指令），但破坏“顶层皆段”的 YAML 风格，拒绝 |
| ~~`server:`~~ | 与 nginx `server{}` 语义冲突，废弃 |

### 8.5 范围收敛

- **本任务**：bootstrap 层对齐 = nginx「main + http 上下文 + `proxy_` 前缀家族的现有交集」的完整翻译（`runtime` 段 + `http.listeners[]` + `proxy.tls` 重命名）。上线后 nginx 运维对 bootstrap 的心智成本趋近于零。
- **后续独立任务**：resources.yaml 对齐审查（routes≈location、upstreams≈upstream、plugins≈模块）——概念已对齐、架构不动，产出是字段级映射表与命名微调。
- **后续独立任务**：迁移文档（`docs/nginx-migration.md` 映射表）。

本任务不碰 resources.yaml，不引入语法兼容。

### 8.6 调研勘误记录

初版映射表漏掉了顶层裸 `tls` 段的对齐缺口：nginx 从无裸 `tls` 上下文，上游 TLS 指令全部带 `proxy_` 前缀。且 `http.listeners[].tls`（入站身份）引入后，同名不同义的问题从潜在变为现实。修正：顶层 `tls` → `proxy.tls`（见 8.2 上游方向表）。
