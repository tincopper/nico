## 1. 已存在的 readiness / 配置状态事实

### 1.1 `ConfigStatus` —— 唯一的运维可见状态结构

`crates/nico-core/src/status.rs:15` 定义 `#[non_exhaustive] pub struct ConfigStatus`，字段全部是值，
不带任何活状态句柄（`crates/nico-core/src/status.rs:11-12` 模块注释即声明这一点）：

| 字段 | `crates/nico-core/src/status.rs:行` | 语义 |
|---|---|---|
| `generation: u64` | `status.rs:17` | 当前已发布快照的代次，首次发布前为 `0` |
| `revision: Option<u64>` | `status.rs:19` | 该快照的 source-scoped revision |
| `digest: Option<String>` | `status.rs:21` | 已发布文档的十六进制摘要（**带堆分配**） |
| `ready: bool` | `status.rs:23` | 「有快照已发布且流水线可服务流量」 |
| `stale: bool` | `status.rs:25` | 该快照来自 last-known-good 缓存 |
| `degraded: bool` | `status.rs:27` | 最近一次 reload 失败，但仍在服务 LKG |
| `last_error_category: Option<ErrorCategory>` | `status.rs:29` | 最近一次失败 reload 的错误类别 |
| `last_reload_unix_secs: Option<u64>` | `status.rs:31` | 最近一次**成功发布**的 unix 秒 |

`ConfigStatus` 通过 `crates/nico-core/src/lib.rs:31` 对外导出，是 `Clone + PartialEq + Eq + Default`。

### 1.2 谁在写这些字段、什么时候写

`SnapshotHandle`（`crates/nico-core/src/snapshot.rs:90`）是两个存储的复合体：

```rust
current: ArcSwapOption<RuntimeSnapshot>,   // snapshot.rs:91  —— 无锁读
status:  Mutex<ConfigStatus>,              // snapshot.rs:92  —— 短临界区
```

- `SnapshotHandle::current()` → `snapshot.rs:107`，`ArcSwapOption::load_full()`，**无锁**。
- `SnapshotHandle::store()` → `snapshot.rs:115`；写 `generation/revision/digest` 并写入
  `last_reload_unix_secs = unix_now()`（`snapshot.rs:124`，`unix_now` 在 `snapshot.rs:175`）。
  **`store` 不改 `ready`**（`snapshot.rs:113-114` 注释、`snapshot.rs:236-240` 测试断言）。
- `SnapshotHandle::status()` → `snapshot.rs:130`：持锁 `clone()`，即每次读都会复制 `digest: String`。
- `SnapshotHandle::update_status()` → `snapshot.rs:139`：闭包在锁内运行，注释明确「绝不跨 `.await`」
  （`snapshot.rs:88-89`）。
- 锁中毒时 `PoisonError::into_inner` 恢复（`snapshot.rs:168-173`）。

状态机的全部写点（`mark_ready` / `mark_degraded` / `mark_stale` / `update_status`）：

| 位置 | 事件 |
|---|---|
| `crates/nico-server/src/reload/publish.rs:106` | 成功发布 → `mark_ready()`（清除 `degraded` 与 `last_error_category`，`snapshot.rs:145-152`） |
| `crates/nico-server/src/reload/publish.rs:110` | 同一次成功发布后单独清 `stale = false` |
| `crates/nico-server/src/reload/publish.rs:169` | 文档/插件/LLM 编译拒绝 → `mark_degraded(category)` |
| `crates/nico-server/src/reload/publish.rs:200-201` | digest 去重命中且状态已 stale/degraded → `mark_ready()` + 清 `stale` |
| `crates/nico-server/src/reload/startup.rs:34` | 启动时源不可用 → `mark_degraded` |
| `crates/nico-server/src/reload/startup.rs:139-142` | 从缓存启动 → `mark_stale()` **且** `mark_degraded(category)` **且** `update_status(|s| s.ready = true)` |
| `crates/nico-server/src/reload.rs:251,348,365,399` | watch 建立失败 / 源失败 / 重新订阅失败 → `mark_degraded` |

### 1.3 「ready」在今天的确切含义

- `ready` 语义 = 「`SnapshotHandle` 里已有一份已发布的快照」。它**单调**：首次发布后没有任何路径把它
  置回 `false`（全仓 grep `ready =` 只有 `snapshot.rs:146` 与 `startup.rs:142` 两处写 `true`）。
- `startup.rs:139-142` 证明 **`ready == true` 与 `degraded == true`、`stale == true` 可以同时成立**：
  从 LKG 缓存启动的进程是 ready 但 degraded 的。`startup.rs:141` 注释明确写出「`mark_ready` 会清除
  `degraded`，而 degraded 正是这里要给运维看的信号」，因此用 `update_status` 直接置 `ready`。
- **真正决定「请求能不能被服务」的事实不是 `ConfigStatus.ready`，而是 `HttpStateHandle`**：
  `crates/nico-http/src/proxy/hooks/request.rs:69-71` ——
  `let Some(state) = app.state.current() else { return app.reject(session, ctx, HttpError::NotReady).await; };`
  即 `HttpStateHandle::current() == None` 时每个请求得到 `503 not_ready`
  （响应体在 `crates/nico-http/src/error.rs:209-213`，错误码 `not_ready`）。
- 发布顺序是 **先 status 后 request path**：`crates/nico-server/src/reload/publish.rs:148`
  （`self.handle.store(...)`）在 `publish.rs:152`（`self.state.publish_with_llm(...)`）之前。
  `publish.rs:129-133` 的注释解释了为什么选这个顺序，但也意味着存在一个（微秒级）窗口：
  `status.ready == true` 而 `HttpStateHandle::current() == None`。
- `last_reload_unix_secs` 只在 `store()` 里更新（`snapshot.rs:124`），`confirm_published`
  （`publish.rs:188-215`，digest 去重命中时）**不更新**它。所以它衡量的是「最后一次实际换代的时刻」，
  不是「最后一次与源确认一致的时刻」。

### 1.4 新端点如何拿到这些事实

- `SnapshotHandle`：进程装配处在 `crates/nico-server/src/server.rs:111-119` 把
  `SnapshotHandle::new()` **内联**传给 `Publication::new(...)`，没有绑定到局部变量，因此当前进程
  外部**没有**任何句柄可以读 `ConfigStatus`。`server.rs:113-115` 的注释原文即为
  "this process has no readiness endpoint yet, so nothing else reads it"。
  要读，只能把该 `Arc<SnapshotHandle>` 先绑定再 clone 一份给端点。
- `HttpStateHandle`：装配处已经持有（`server.rs:86`：`let state = HttpStateHandle::new();`，
  并在 `server.rs:116` `Arc::clone(&state)`、`server.rs:128` 移交给数据平面服务）。
  `HttpState::snapshot()` → `crates/nico-http/src/state.rs:61` 可直接给到
  `generation` / `revision` / `digest` / `source` / `document()`，**但不含** `ready/stale/degraded`。
- `Publication`（`crates/nico-server/src/reload.rs:61-69`）是二者的成对构造器，注释明确二者
  「一起写、一起给」（`reload.rs:55-60`）。

### 1.5 代次类型

`Generation::ZERO = 0`（`crates/nico-core/src/version.rs:22`），`next()` 溢出返回
`GenerationOverflow`（`version.rs:52`）。`RevisionOrdering`（`version.rs:67`）区分可比序与仅等价序；
`Revision::equality_only` 在缓存启动时使用（`startup.rs:127`）。

---

## 2. 第二个监听器 / admin 服务如何挂上

### 2.1 现状：只有一个服务 + 一个后台服务

`crates/nico-server/src/server.rs:120-128`：

```rust
server.add_service(background_service("config-reload", ReloadService { .. }));   // 120-127
server.add_service(http_service(&server.configuration, state, settings));        // 128
```

`server.configuration` 来自 `server_conf(settings)`（`server.rs:102`），`settings` 来自
`bootstrap.http()`（`server.rs:83`：`let settings = bootstrap.http();`）。

`crates/nico-http/src/service.rs:66-82` 的 `service()`：

```rust
let app = HttpApp::new(state);                    // service.rs:71
let mut options = HttpServerOptions::default();   // service.rs:75
options.h2c = settings.h2c();                     // service.rs:76
let mut service = ProxyServiceBuilder::new(conf, app).server_options(options).build();  // 77-79
service.add_tcp(settings.listen());               // service.rs:80
```

`server_conf()`（`service.rs:49-58`）只设置 `grace_period_seconds`、`graceful_shutdown_timeout_seconds`、
`threads`、`max_retries = MAX_ATTEMPTS_PER_ROUTE`，其余用 `ServerConf::default()`。

### 2.2 Pingora 侧的可选挂载方式（源码证据来自 `pingora-core 0.9.1`）

- `Server::add_service(service: impl ServiceWithDependents + 'static) -> ServiceHandle`
  （`~/.cargo/registry/src/*/pingora-core-0.9.1/src/server/mod.rs:546`）。当前仓内两处调用都丢弃了
  返回值（`server.rs:120`、`server.rs:128`），因此可自由新增服务；`nursery`/`unused_must_use` 不受影响
  （`Server::add_service` 无 `#[must_use]`）。
- `pub trait Service: Sync + Send`（`pingora-core-0.9.1/src/services/mod.rs:404`）：只需实现
  `fn name(&self) -> &str`（`services/mod.rs:429`），`start_service(&mut self, fds, shutdown,
  listeners_per_fd)` 有默认空实现（`services/mod.rs:417-424`）。**因此可以直接实现 `Service` trait**。
- `services::listening::Service<A>`（`pingora-core-0.9.1/src/services/listening.rs:47`）要求
  `A: ServerApp`；其 `start_service` 通过 `current_handle()` 拿到本服务的 runtime 并
  `spawn` accept 循环（`listening.rs:278-299`，`current_handle()` 在 `listening.rs:285`）。它提供
  `add_tcp(&mut self, addr: &str)`（`listening.rs:135-137`）和 `pub threads: Option<usize>`
  （`listening.rs:52`）。名字上限 16 字符（`services/mod.rs:315-316`）。
- 最轻的 HTTP 承载路径：`pingora::apps::HttpServer::new_app(SV)`（`pingora-core-0.9.1/src/apps/http_app.rs:121-128`）
  其中 `SV: ServeHttp`（`http_app.rs:32-38`，唯一方法
  `async fn response(&self, http_session: &mut ServerSession) -> Response<Vec<u8>>`），
  `HttpServerApp` 自动获得实现（`http_app.rs:137-141`）。`pingora` 门面 crate 通过
  `pub use pingora_core::*;`（`pingora-0.9.1/src/lib.rs:37`）导出 `apps` / `services`。
  仓内目前只用到 `pingora::services::listening::Service`（`crates/nico-http/src/service.rs:24`），
  从未使用 `ServeHttp` / `HttpServerApp`。
- `HttpProxy<HttpApp, ()>` 路线需要实现完整 `ProxyHttp`（`crates/nico-http/src/proxy/hooks.rs:60-168`
  是现成的完整实现参考），对一个只答固定路径的端点来说接口远大于需求。

### 2.3 同一服务加两个 `add_tcp` 的后果

`listening::Service::add_tcp` 可以调用多次，但一个 `Service` 只有一个 `app_logic`
（`listening.rs:47-54`，`app_logic: Option<A>` 在 `start_service` 里被 `take()`，`listening.rs:296-299`）。
因此「在同一个 `HttpApp` 服务上再 `add_tcp(admin_addr)`」会让 admin 端口也走数据平面的
`ProxyHttp` 全套 hook —— 即 admin 路径必须是一条 route，且能被文档配置覆盖。仓内没有任何
路径白名单或「本地端口」概念可复用。

### 2.4 bootstrap 里没有第二个地址的位置

- `crates/nico-server/src/bootstrap.rs:217-227` 的 `RawBootstrap` 是
  `#[serde(deny_unknown_fields)]`，字段固定为
  `config_source` / `schema_bounds` / `http` / `debounce_ms` / `max_wait_ms` / `cache_dir` /
  `allow_bootstrap_cache`。**没有 admin / metrics / ops 段。**
- `crates/nico-server/src/bootstrap/http.rs:20-26` 的 `RawHttp` 同样是 `deny_unknown_fields`：
  `listen` / `h2c` / `grace_period_ms` / `graceful_shutdown_ms` / `threads`。
- `crates/nico-http/src/settings.rs:45-51` 的 `HttpSettings` 只有**一个** `listen: String`，
  且只能由 `HttpSettings::resolve(...)`（`settings.rs:64-107`）构造，`resolve` 的签名是
  `(listen, h2c, grace_period_ms, graceful_shutdown_ms, threads)`（`settings.rs:64-71`）。
- `crates/nico-server/src/bootstrap.rs:132-135` 的 `BootstrapConfig::http()` 返回 `&HttpSettings`。
- 结论：**第二个 bind 地址是全新的配置面**，且 `deny_unknown_fields`（`bootstrap.rs:218`、
  `bootstrap/http.rs:20`）意味着不加字段就写不进去；写进去就必须同时改
  `RawHttp` → `HttpSettings::resolve` 的校验链（`settings.rs:64-107`，含 `validate_listen`
  `settings.rs:216-234` 的 port != 0 与形状检查）。
- `config/bootstrap.yaml` 只文档化了现有的 `http.listen` 一项。

---

## 3. metrics 候选 crate 与依赖策略

### 3.1 规范已经预先指定了 crate 家族与指标名

`.trellis/spec/backend/logging-guidelines.md:239-241`：
> "Metrics are separate from logs: use `metrics` + an exporter configured in `nico-server`. The global
> recorder is installed once at startup (`OnceLock`/`LazyLock`), never re-initialized on reload."

`.trellis/spec/backend/logging-guidelines.md:247-250`：
> "…The `metrics` crate and an exporter arrive with the phase-1 data plane; until then these names are
> the contract to emit against."

`.trellis/spec/backend/logging-guidelines.md:251-253` 给出配置子系统基线指标名：
`config_load_total{source,result}`、`config_reload_total{result}`、`config_compile_seconds`、
`config_watch_reconnects_total`、`config_snapshot_age_seconds`、`config_degraded`。

命名与标签预算：`logging-guidelines.md:242-246`（`gateway_<subsystem>_<noun>_<unit>`，label 允许
`source` / `result` / `route_id` / `provider` / `status_class` / `error_category`）。
禁止的架构：`logging-guidelines.md:16-17`「不要为一个消费者建 `ExporterPlugin`/`MetricsRegistry` 抽象」。

### 3.2 Cargo.lock 中不存在任何 metrics 相关 crate

`Cargo.lock` 共 238 个 package；对 `metrics` / `metrics-exporter-prometheus` / `prometheus` /
`prometheus-client` / `opentelemetry` 的检索全部为空（`grep -n '^name = ' Cargo.lock` 的完整列表里
没有这些名字）。**即：这些 crate 目前既没有源码访问，也没有被 `cargo deny` 校验过。**

`Cargo.toml:12-50` 的 workspace 依赖是**精确 pin**（`=x.y.z`，注释指向
`.trellis/tasks/11-03-phase0-tech-verification/design.md` 第 2 节），例如
`arc-swap = "=1.9.3"`（`Cargo.toml:14`）、`pingora = "=0.9.1"`（`Cargo.toml:29`）、
`tokio = { version = "=1.54.1", … }`（`Cargo.toml:41`）。新增依赖必须进 workspace 表（惯例，非硬规则）。

### 3.3 deny.toml 的许可证白名单

`deny.toml:30-42`：`allow = ["Apache-2.0", "MIT", "ISC", "Unicode-3.0", "CC0-1.0", "BSD-3-Clause", "Zlib"]`，
`confidence-threshold = 0.93`（`deny.toml:43`）。`[sources]` 只允许 crates.io
（`deny.toml:49-52`），`unknown-registry = "deny"`、`unknown-git = "deny"`。`[bans] multiple-versions = "warn"`、
`wildcards = "deny"`（`deny.toml:46-47`）。

候选 crate 的**许可证声明未在仓内找到**（仓内没有 vendored 源码、没有 Cargo.lock 条目）。
可以从白名单推出的是判定规则，而非候选 crate 的结论：

- 任何以 `MIT` 或 `Apache-2.0`（含二者双许可）发布的 crate **可以通过**；
- 任何以 `MPL-2.0`、`BSL-1.0` 等未列入白名单的许可证发布的 crate **会失败**，且 `deny.toml` 无
  `[licenses.exceptions]` 段可放行。

`cargo deny check` 是 pre-push 门禁（`scripts/gates/pre-push.sh:54-57`），不是可选步骤。
`~/.cargo/registry` 里存在 `prometheus-0.13.4`，但它**不是**本 workspace 的依赖，不能作为证据。

### 3.4 crate 边界规范

- 依赖方向图（`.trellis/spec/backend/directory-structure.md:209-220`）：
  `nico-server → {nico-core, nico-http, nico-llm, nico-config, nico-plugins}`。
- 硬规则 `directory-structure.md:222-228`：**`nico-core` 不得依赖 `nico-http` / `nico-llm` /
  `nico-plugins` / `pingora` / `hyper` / 任何 config-center 客户端**，「不带网络客户端、不带策略密码学」。
  当前 `crates/nico-core/Cargo.toml` 的依赖只有 `arc-swap, serde, serde_norway, sha2, thiserror`
  —— **没有 tokio**。
- `directory-structure.md:230-247`：`nico-http` 不得 import `nico-llm` / `nico-plugins`，不得 re-export
  Pingora 类型，不得安装信号处理器或自建 runtime（「那是进程装配，住在 `nico-server`」）。
- `directory-structure.md:202-203`：「If you cannot name the second real consumer of an abstraction,
  it does **not** belong in `nico-core`.」
- 模块边界优先于 crate 边界：`directory-structure.md:11-13`「Module before crate — start inside a
  single crate. Split into a new crate only when a real boundary appears」。
- `nico-core` 是 `ConfigStatus` 的拥有者（`crates/nico-core/src/lib.rs:31`），`nico-http` 是
  `HttpSettings` 的拥有者（`crates/nico-http/src/lib.rs:121`），`nico-server` 是「唯一知道所有具体实现」
  的 crate（`directory-structure.md:16-17`）。

### 3.5 结构测试 / 依赖断言：新依赖会不会踩线

仓内只有一个会读 `Cargo.toml` 的结构测试：`crates/nico-llm/tests/structure.rs:140-154`
（`the_http_data_plane_does_not_depend_on_this_crate`），断言 `nico-http/Cargo.toml` 文本里不含
`"nico-llm"`。给 `nico-http` 或 `nico-server` 加 metrics crate **不会**触发它。

vendor-free 名称检查（`crates/nico-llm/tests/structure.rs:18` `VENDORS`、`:26-46` `VENDOR_FREE` 的 17 个
路径）只扫描 `nico-llm/src` 下的指定文件与目录，检查 `openai|anthropic|gemini|claude|gpt` 字面量
（`structure.rs:92-121`）。journal 记录的「结构测试抓到 vendor 名」事件即指这一条
（`.trellis/workspace/tincopper/journal-1.md:325`）。metrics crate 与这条规则无关。

其余结构断言：`crates/nico-llm/tests/structure.rs:125-138`（vendor 只在 module-wiring 行）、
`:156-203`（registry key 拼写契约）。**没有任何测试断言 crate 依赖清单本身。**

---

## 4. 现有「天然可暴露」的计数 / 限额状态

### 4.1 请求生命周期：唯一的「一次调用即记录」缝隙

`crates/nico-http/src/proxy/hooks/response.rs:279-293`：

```rust
pub(super) async fn logging(_app: &HttpApp, session: &mut Session, error: Option<&Error>, ctx: &mut HttpContext) {
    ... chain.run_logging(ctx, head).await;            // response.rs:286-291
    record::emit(session, error, ctx);                 // response.rs:292
}
```

`record::emit`（`crates/nico-http/src/proxy/record.rs:160`）在同一处已经算出并可用：

| 事实 | 来源 |
|---|---|
| `status`（含 log-only `499`，`record.rs:26`） | `record.rs:168-176` |
| `cancelled` | `record.rs:168-169` |
| `elapsed_ms` | `ctx.elapsed_ms(Instant::now())`（`record.rs:171` → `context.rs:275`） |
| `route_id` | `context.rs:148` |
| `upstream`（upstream 名） | `context.rs:163` |
| `generation` | `ctx.generation()` → `context.rs:142` |
| `trace_id` | `context.rs` 内部字段 |
| `body_bytes` | `context.rs:299` |
| `error.category` | `ctx.error()` → `context.rs:331` |

这是**每请求恰好一次**（Pingora `logging` hook，`hooks.rs:18` 表格末行）。在 `record::emit` 内插入一次
counter/histogram 记录不需要新增 hook、不需要新增锁、不改变 `HttpApp`（`proxy.rs:58-62`，
`HttpApp` 只有 `state` + `sequence` 两个字段，`proxy.rs:59-61`）。
`record.rs` 当前 269 行（`wc -l`），离 500 行硬顶有余量。

### 4.2 熔断器表：状态齐全，但进程装配处拿不到

`crates/nico-llm/src/breaker.rs:216` `pub struct BreakerTable { entries: Mutex<BTreeMap<TargetIdentity, Breaker>> }`。

- `admit(&self, identity, now) -> Admission`（`breaker.rs:233`）
- `record(&self, identity, outcome, failure, now)`（`breaker.rs:248`）
- `last_error(&self, identity)`（`breaker.rs:269`）
- `retain(&self, live)`（`breaker.rs:281`，发布时按 live identity 剪枝）
- `log_transition`（`breaker.rs:300-312`）：`closed → open → half_open` 的唯一日志点，字段
  `llm.target` + `llm.circuit`，`warn!`。
- `BreakerPhase::as_str()`（`breaker.rs:63-73`）已经给出可作 label 的三个值
  `closed` / `open` / `half_open`。

**可观测性缺口（关键事实）**：进程装配处从未持有这张表。`crates/nico-server/src/llm/shared.rs:18-39`
把 `Arc<BreakerTable>` 收在 `SharedServices` 里并给出 `breakers()`（`shared.rs:38`），但
`SharedServices` 由 `LlmRuntime::with_parts` 内部创建（`crates/nico-server/src/llm/runtime.rs:51-72`、
`shared.rs:18-22`），而 `server.rs` 调用的 `Reloader::new(...)`（`reload.rs:184-195`）内部走
`builtin_llm()` → `LlmRuntime::builtin()`（`reload/llm.rs:116-119`、`llm/runtime.rs:36-41`）。
即：**`server.rs` 没有 `LlmRuntime` 或 `SharedServices` 的句柄**，只有
`Reloader::with_llm`（`reload.rs:205-236`）这个注入点可以传进一个自建的 `Arc<dyn LlmCompiler>`。
`BreakerTable` 的 `Debug` 只输出 `targets: usize`（`breaker.rs:317-326`），没有按 identity 枚举的只读
访问器 —— 要 scrape 每个 target 的 phase，需要新增只读方法。

### 4.3 限流表：完全不可达

`crates/nico-plugins/src/rate_limit/limiter.rs:96` `pub(crate) struct Limiter`，`acquire` 在
`limiter.rs:125`（同步、短临界区、不跨 await）。
访问器 `len()` 是 **`#[cfg(test)]`**（`limiter.rs:151-154`）。
表本身由 `RateLimitFactory` 私有持有：`crates/nico-plugins/src/rate_limit/factory.rs:29-31`
`tables: Mutex<HashMap<String, Weak<Limiter>>>`，`current()` 同样 `#[cfg(test)]`
（`factory.rs:61-64`）。`PluginRuntime`（`crates/nico-plugins/src/compile.rs:135-139`）**只持有
`PluginRegistry`**，没有任何通往 factory 内部状态的访问器（`compile.rs:141-155`）。
→ 暴露「每个 rate_limit 插件当前 bucket 数」需要穿透 `PluginRuntime` → `PluginRegistry` →
具体 `PluginFactory` 三段，全部新增公开面。

### 4.4 LLM 尝试 / 重试的可记录点

- `AttemptSequence::classify`（`crates/nico-llm/src/route/attempt.rs:238`）、
  `transport_failed`（`attempt.rs:269`）、`succeeded`（`attempt.rs:289`）三处最终都落到
  `leave_current` → `breakers.record(...)`（`attempt.rs:390-399`）。这是 attempt 级结果唯一的写点。
- `attempt_number()`（`attempt.rs:315`）与 `budget()`（`attempt.rs:303`）是现成的计数事实。
- 请求侧的 attempt 记录在 `crates/nico-http/src/proxy/hooks/request.rs:230-270`：
  `llm.target` + `llm.attempt` + `llm.outcome="skipped"`，且 `llm.note_attempt_started(&name, number)`
  （`request.rs:269`）就在同一处。

### 4.5 会被「锁」政策挡住的位置

- 所有可记录点都已经是**同步、有界临界区**（`snapshot.rs:88-89`、`breaker.rs:32-34`、
  `limiter.rs:15-16`）。在它们**内部**再取一把锁会把临界区扩大，违反
  `logging-guidelines.md:16-17` 的「不为主要消费者造抽象」和不跨 await 的既有约定。
- `clippy::await_holding_lock` 是 `deny`（`Cargo.toml:73`），任何在持锁期间 `.await` 的改动都
  编不过。
- `status()` 每次 clone `digest: String`（`snapshot.rs:130`）—— 抓取频率下可接受，但它是
  分配，不是无锁。

---

## 5. 测试缝隙与约定

### 5.1 端到端 HTTP 测试如何起服务

`crates/nico-server/tests/support/http.rs`（252 行）提供：

| 辅助 | 位置 | 作用 |
|---|---|---|
| `free_port() -> u16` | `support/http.rs:76-84` | bind 0 取端口后释放；bootstrap 拒 `port == 0`，Pingora 不回报告实际端口 |
| `write_bootstrap(dir, port, h2c, grace_ms, shutdown_ms)` | `support/http.rs:88-124` | 写 bootstrap.yaml 并 `BootstrapConfig::load`；`debounce_ms: 0`、`max_wait_ms: 0` |
| `RunningServer::start(dir, source)` | `support/http.rs:136-142` | 在新线程跑**生产装配** `server::serve_with(...)`，`wait_until_accepting()` 轮询 20s（`support/http.rs:36` `STARTUP_BUDGET`） |
| `RunningServer::start_with_registry(...)` | `support/http.rs:144-167` | 同上，但注入自定义 `PluginRegistry` |
| `RunningServer::cancel_and_join() -> ExitCode` | `support/http.rs:183-198` | cancel token + `spawn_blocking(join)` |
| `wait_for_status(addr, &Request, status)` | `support/http.rs:231-252` | 反复请求直到某状态码出现（文档发布是异步的，没有可 await 的句柄） |
| `resources::Resources` / `Route` / `Plugin` | `support/http/resources.rs` | 完整 resources 文档 builder（schema 要求四个 section 都在） |
| `source::TestSource` | `support/http/source.rs` | 测试可推送文档的配置源 |
| `upstream::MockUpstream` / `client::Client` / `mock` | `support/http/upstream.rs`、`client.rs`、`mock.rs` | 手写 mock upstream / 手写 client / 仅测试注册的 plugin kind |

用法示例：`crates/nico-server/tests/http_shutdown.rs:43-60`（`free_port` → `write_bootstrap` →
断言 `bootstrap.http().grace_period()` → `RunningServer::start`）。测试 runtime 是
`#[tokio::test(flavor = "current_thread")]`（`http_shutdown.rs:46`）。

**新增一个 readiness / metrics 端点测试可以直接复用的最小集合**：
`free_port`（`support/http.rs:76`）、`write_bootstrap`（`support/http.rs:88`）、
`RunningServer::start`（`support/http.rs:136`）、`client::exchange`（`crates/nico-server/tests/support/http/client.rs`，被
`wait_for_status` 在 `support/http.rs:238` 调用）。
注意 `write_bootstrap` 的签名是**位置参数**（`support/http.rs:88-94`），加一个 admin 端口意味着改签名
或新增一个函数 —— 现有 4 个参数已被 20+ 测试文件使用。

### 5.2 唯一的 pipeline 层 seam

`crates/nico-server/tests/support/mod.rs:306-443` 的 `Harness` 直接构造
`Reloader::new(source, Publication::new(handle, state), PluginRuntime::new(), settings)`
（`support/mod.rs:348-378`），并暴露 `handle()`（`support/mod.rs:380`）、`status()`
（`support/mod.rs:389`）、`ready()`（`support/mod.rs:397`）、`generation()`（`support/mod.rs:393`）、
`advance_until()`（`support/mod.rs:419`）。它跑在 paused clock 上，**不建 socket**
（`support/mod.rs:9-11`）。断言 readiness 的「状态机」部分可以在这里做，不需要 socket；
断言 HTTP 形状必须在 `support/http.rs` 的世界里做。

`crates/nico-server/tests/publication.rs`（157 行）与 `crates/nico-server/tests/support/mod.rs:389,397`
的 `status()` / `ready()`
是现有的 `ConfigStatus` 断言范例。

### 5.3 文件预算与将要改的文件的当前行数

规则：目标 300–500 行，**硬顶 500，含内联 `#[cfg(test)]` 模块**
（`.trellis/spec/backend/quality-guidelines.md:100-118`）；pre-commit 强制
（`scripts/gates/pre-commit.sh:110-166`，超顶文件「只能缩小」）。超顶检测命令在
`quality-guidelines.md:115-118`。

| 文件 | 行数 | 备注 |
|---|---|---|
| `crates/nico-server/src/server.rs` | 194 | 加第二个服务的主要落点，余量充足 |
| `crates/nico-server/src/lib.rs` | 308 | 模块声明与重导出 |
| `crates/nico-server/src/reload.rs` | **459** | **已接近硬顶**；`Publication` 若需扩展必须谨慎 |
| `crates/nico-server/src/reload/publish.rs` | 255 | |
| `crates/nico-server/src/reload/startup.rs` | 155 | |
| `crates/nico-server/src/bootstrap.rs` | 387 | 加新配置段就在此 |
| `crates/nico-server/src/bootstrap/http.rs` | 62 | |
| `crates/nico-http/src/service.rs` | 91 | 第二服务的 builder 天然落点 |
| `crates/nico-http/src/settings.rs` | 241 | `HttpSettings` 只有单 `listen` |
| `crates/nico-http/src/proxy/record.rs` | 269 | 指标记录点，余量充足 |
| `crates/nico-http/src/proxy/hooks/response.rs` | 293 | |
| `crates/nico-server/tests/support/http.rs` | 252 | 测试辅助，余量充足 |
| `crates/nico-server/tests/pipeline.rs` | **496** | 已接近硬顶 |

全仓最大的文件是 `crates/nico-llm/tests/anthropic_provider/response.rs` = 500 行（正好在顶），
其余均在 500 以下（`find crates -name '*.rs' | xargs wc -l | sort -rn | head`）。

---


---

由这些事实推出的设计约束见 `ops-surface-constraints.md`。
