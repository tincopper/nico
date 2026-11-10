## 设计约束（由事实推出）

1. **readiness 必须与请求路径读同一个事实，否则会撒谎。** 请求能不能被服务只由
   `HttpStateHandle::current()` 决定（`crates/nico-http/src/proxy/hooks/request.rs:69-71`），而
   `ConfigStatus.ready` 是另一处、且先于 request path 写入（`reload/publish.rs:148` 早于 `:152`）。
   端点若只读 `ConfigStatus.ready`，存在「报 ready 而请求 503」的窗口。

2. **`ready` 不能等同于「健康」。** 从 LKG 启动的进程 `ready == true && degraded == true &&
   stale == true` 三者同时成立（`reload/startup.rs:139-142`）。任何 readiness 判定必须同时
   消费 `degraded` / `stale`，否则「配置源已死、只服务一份记忆」的进程会被 k8s 判为可接流量。

3. **`ready` 是单调的，`degraded` 不是。** `ready` 只在两处置 `true`、从不置回 `false`
   （`snapshot.rs:146`、`startup.rs:142`）；`degraded` 由每次失败 reload 置位（`reload.rs:251,348,365,399`、
   `publish.rs:169`），由 `mark_ready` 或 `confirm_published` 清除（`snapshot.rs:145-152`、
   `publish.rs:200`）。因此 liveness 与 readiness 必须映射到不同字段。

4. **进程今天拿不到自己写的 `SnapshotHandle`。** 它在 `crates/nico-server/src/server.rs:116` 被内联
   构造并移交，`server.rs:113-115` 明确记录「no readiness endpoint yet」。任何端点都要先把该
   `Arc<SnapshotHandle>` 绑定到局部变量并 clone 一份。

5. **进程今天也拿不到 `BreakerTable`。** 它藏在 `LlmRuntime::builtin()`（`reload/llm.rs:116-119`）内部，
   `server.rs` 没有 `LlmRuntime` / `SharedServices` 句柄；唯一注入点是
   `Reloader::with_llm`（`reload.rs:205`）。要 scrape 熔断状态，必须在装配处自建 `LlmRuntime` 并
   经 `with_llm` 传入，同时给 `BreakerTable` 新增只读枚举方法（现有 `Debug` 只给 `targets: usize`，
   `breaker.rs:317-326`）。

6. **限流表不可观测是结构性的，不是遗漏。** `Limiter::len()` 与 `RateLimitFactory::current()` 都是
   `#[cfg(test)]`（`limiter.rs:151-154`、`factory.rs:61-64`），`PluginRuntime` 只持 `PluginRegistry`
   （`compile.rs:135-139`）。暴露 bucket 数需要新增三段公开面 —— 应作为「第二个消费者」判断后再做，
   符合 `directory-structure.md:202-203` 与 `logging-guidelines.md:16-17`。

7. **第二个监听端口是全新配置面，且必须是可选段。** `RawBootstrap` 与 `RawHttp` 都是
   `deny_unknown_fields`（`bootstrap.rs:218`、`bootstrap/http.rs:20`），`HttpSettings` 只有一个
   `listen`（`settings.rs:46`），且只能经 `resolve(...)` 构造（`settings.rs:64-107`）。
   新增字段必须同时走 `RawHttp::resolve` 的校验链（`bootstrap/http.rs:35-49`），并由
   `validate_listen`（`settings.rs:216-234`）拒绝 `port == 0`。

8. **admin 端点不应复用数据平面的 `ProxyHttp`。** 一个 `listening::Service` 只有一个 `app_logic`
   （`listening.rs:47-54`、`:296-299`），因此「同一服务加第二个 `add_tcp`」会让 admin 端口也走
   `HttpApp` 全套 hook，使 `/metrics` 变成一条可被文档覆盖的 route。而 Pingora 0.9.1 已提供
   `ServeHttp` + `HttpServer::new_app`（`apps/http_app.rs:32-38,121-128`）这条只要一个
   `Response<Vec<u8>>` 的轻路径，`Service` trait 也可直接实现（`services/mod.rs:404-470`）。

9. **`nico-core` 不能吸收 metrics 依赖。** `directory-structure.md:222-228` 禁止 `nico-core` 依赖
   `nico-http` / pingora / hyper 等，其 `Cargo.toml` 连 tokio 都没有。`ConfigStatus` 住在
   `nico-core`（`lib.rs:31`），所以「把状态转成 metric sample」的代码只能住在 `nico-server`
   （或 `nico-http`，若它需要记录请求侧计数）。

10. **metrics crate 的选择已被规范预定，但许可证未验证。** `logging-guidelines.md:239-241` 指定
    `metrics` + 在 `nico-server` 里配置的 exporter，`:251-253` 指定指标名。但
    `Cargo.lock` 里没有任何 metrics crate，`deny.toml:30-42` 的白名单只允许
    Apache-2.0/MIT/ISC/Unicode-3.0/CC0-1.0/BSD-3-Clause/Zlib，且无 `exceptions` 段。
    候选 crate 的实际许可证 **未在仓内找到**，必须在实现前核实，否则 pre-push 的
    `cargo deny check`（`scripts/gates/pre-push.sh:54-57`）会红。

11. **请求侧指标只有一个正确插入点。** `record::emit`（`crates/nico-http/src/proxy/record.rs:160`）
    是每请求恰好一次、已经算好 `status`/`route_id`/`upstream`/`elapsed_ms`/`error.category`/
    `generation` 的唯一位置（调用点 `proxy/hooks/response.rs:292`）。在其他 hook 里记录会重复计数或
    遗漏 `fail_to_proxy` 路径。该文件 269 行，加一个记录调用不触碰 500 行硬顶。

12. **不得在既有锁内新增记录。** `SnapshotHandle::status()` 持 `Mutex` 且 clone `digest: String`
    （`snapshot.rs:130`），`BreakerTable` 与 `Limiter` 的所有方法都在同步短临界区内
    （`breaker.rs:32-34`、`limiter.rs:15-16`），且 `clippy::await_holding_lock` 是 `deny`
    （`Cargo.toml:73`）。指标记录必须是锁外的一次调用。

13. **默认关闭是硬要求。** `config/bootstrap.yaml` 只有 `http.listen` 一个地址；若新增 admin 端口，
    缺省必须是「不监听」，否则现有 20+ 个 `write_bootstrap` 夹具
    （`support/http.rs:88-124`）会因为端口冲突全部失败。

14. **测试落点已经存在。** 端到端用 `free_port` / `write_bootstrap` / `RunningServer::start` /
    `client::exchange`（`support/http.rs:76,88,136`；`support/http/client.rs`），状态机用 `Harness::status()` / `ready()`
    （`support/mod.rs:389,397`）在 paused clock 上断言。`write_bootstrap` 是位置参数签名
    （`support/http.rs:88-94`），改签名会波及全部 HTTP 测试，新增一个函数更省。

