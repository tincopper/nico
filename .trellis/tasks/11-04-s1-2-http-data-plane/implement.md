# gateway-http：Pingora 数据面骨架 — 实施计划

> 每一步结束都必须 `cargo test --workspace` 全绿，且被改文件稳定在 500 行以内（目标 300–500）。
> 步骤顺序即依赖顺序：先纯逻辑（可单测），再接 Pingora（只能集成测），最后接进程组装。
> 设计依据见 `design.md`；Pingora 行为结论都来自 `pingora 0.9.1` 源码，改动版本时必须复验。

## 0. 前置确认

- [x] `1.1 route-schema` 已归档：`Document.routes()/upstreams()/providers()` 与 `Bound` 可用。
- [x] 读 `design.md` §1 的六条不变量；实施中任何一步若与之冲突，停下来改设计而不是绕过。
- [x] 确认 `pingora = "=0.9.1"` 已在本地 registry cache（离线可构建）。

## 1. crate 骨架与依赖边界

- [x] 新建 `crates/gateway-http`：`Cargo.toml`（`gateway-core.workspace`、`pingora = "=0.9.1"`、
      `thiserror`、`tracing`、`bytes`、`async-trait`、`tokio`；dev：`tokio`、`sha2`、`tempfile` 按需）。
      不直接依赖 `http` crate：`StatusCode`/`Method`/`RequestHeader` 一律取 `pingora::http`。
      workspace `Cargo.toml` 增加 `pingora = "=0.9.1"` 到 `[workspace.dependencies]`。
- [x] `lib.rs`：`//!` 说明 crate 边界（Pingora 适配 + HTTP 领域 trait，不含 AI/策略）、`#![deny(missing_docs)]`、
      重导出 `HttpError`/`Respond`/`HttpFilter`/`FilterChain`/`HttpContext`/`HttpApp`/`match_route`/`body_limit`
      （`peer` 与 `host_header` 留在 `upstream` 模块内，不从根重导出）。
      HTTP 类型取自 `pingora::http`，不直接依赖 `http` crate。
- [x] `gateway-server` 增加 `gateway-http` 依赖（本步只用不接）。
- [x] 门禁：`cargo check --workspace`；`cargo tree -p gateway-core | grep -E 'pingora|gateway-http'` 无输出；
      `cargo tree -p gateway-http | grep -E 'gateway-ai|gateway-plugins'` 无输出。

## 2. `error.rs`：唯一失败词汇表

- [x] `HttpError`（`thiserror`，`#[non_exhaustive]`，`Debug`），变体与 `design.md` §9 表格一致：
      `NotReady` / `RouteNotFound` / `InvalidRequest { reason }` / `BodyTooLarge { limit }` /
      `DeadlineExceeded { elapsed_ms }` / `UpstreamFailed { source }` / `ProviderNotConfigured { name }` /
      `ResponseCommitted`。
- [x] `Respond { status, code, message }`（字段私有 + accessor，三者都是常量）+
      `fn respond(&self) -> Option<Respond>`：唯一映射点；`ResponseCommitted` 返回 `None`（无处可写）。
- [x] `ErrorCategory` 映射与 `Send + Sync + 'static` 编译期断言（照 1.1 的 `const _: fn()` 写法）。
- [x] 响应体拼装 `Respond::body(trace_id) -> Bytes`：`{"error":{"code","message","trace_id"}}`，
      常量字段直接写入，只有可控的 `trace_id` 做 JSON 转义；不含上游名/地址/凭证。
- [x] 测试：每个变体的 status/code/category 三元组逐项断言；响应体不重复 `source` 或 `InvalidRequest.reason`；
      trace id 注入 `"`/控制字符后仍是合法 JSON。
- [x] 门禁：单元测试通过；clippy 无 `unwrap/expect`（测试外）。

## 3. `route.rs`：匹配规则

- [x] `pub fn match_route(header: &RequestHeader, routes: &[Route]) -> Option<&Route>`（自由函数，无命名空间结构体）。
- [x] host 规则：`None` 匹配任意；`Some(h)` 与请求 `Host` 去端口后大小写不敏感相等。
      取 host 的顺序与 Pingora 自己的协调顺序一致：URI authority → raw-target authority → `Host` 头
      （HTTP/1 absolute-form 的 authority 只在 `raw_path()` 里，测试发现过这一点）。
- [x] 优先级：host 具体 > 任意；再比 `path_prefix` 字节长度；相等时保持文档顺序（先出现者胜）。
- [x] 单次扫描、零分配、无 `clone`。
- [x] 测试：最长前缀、host 精确优先于通配、前缀边界（`/api/` 不匹配 `/api`、`/apix` 命中 `/api`）、
      无匹配、大小写与 `:port`、absolute-form 优先于冲突的 `Host`、等长前缀取先出现者。
- [x] 门禁：单元测试通过。

## 4. `upstream.rs`：目标 → peer

- [x] `fn peer(upstream: &Upstream, budget: Duration) -> Result<Box<HttpPeer>, HttpError>`：先解析，
      再 `HttpPeer::new(SocketAddr, tls, sni)`；`options.connection_timeout`（字段名如此）与 `read_timeout`
      = budget；ALPN 保持默认 H1（注释说明 gRPC 需显式 `ALPN::H2`）。可失败的理由：`HttpPeer::new((host,port),..)`
      对解析结果 `unwrap()`，先解析把 panic 变成 502。
- [x] `fn host_header(upstream: &Upstream) -> String`（host + `:port`，默认端口省略；未知 scheme 一律写出端口）。
- [x] `provider` 目标不走这里：`proxy.rs` 直接返回 `ProviderNotConfigured`。
- [x] 测试：明文/TLS 的 `tls` 与 `sni`、`Host` 头的端口规则、超时预算透传、解析失败映射为 502、ALPN 仍为 H1。
- [x] 门禁：单元测试通过。

## 5. `context.rs` + `limits.rs`

- [x] `HttpContext`：`route_id`、`upstream`、`config.generation`、`method`、`path`（去查询串）、
      `started`/`deadline: Instant`、`body_limit`、`body_bytes`、`error: Option<HttpError>`、`cancelled`。
      没有 `committed` 字段——「已提交」由 `session.response_written()` 回答。
      `begin(&Arc<RuntimeSnapshot>, &Route, &RequestHeader, now: Instant)`：`&Arc` 避免与 `&Route` 借用冲突，
      注入 `now` 让 deadline 测试不需要 sleep。
- [x] `limits.rs`：`body_limit`、`request_budget`（同一条「route 只能收紧」规则）+ `over_limit`；
      三个函数被预检、累计、peer 预算三处共用。
- [x] 测试：较小者语义（route 未设、route 更小、文档更小）、边界值（等于上限不算超）、剩余预算饱和到 0。
- [x] 门禁：单元测试通过。

## 6. `filter.rs`：扩展点（1.2 空链）

- [x] `#[async_trait] pub trait HttpFilter`：`name`、`on_request`、`on_response`，签名与 `design.md` §8 一致。
- [x] `FilterChain::new(Arc<[Arc<dyn HttpFilter>]>)` + `run_request` / `run_response`：顺序执行，
      首个 `Err` 短路并原样返回；空链与 `Default` 都不分配。
- [x] 测试：两个阶段各自的执行顺序（filter 能看到 ctx）、首个错误短路、空链、filter 改写两阶段头。
- [x] 门禁：单元测试通过；`HttpFilter` 的 doc 写清「拒绝即错误，不要自己写响应」。

**门 A 记录（已完成）**：`gateway-http` 7 个模块 + `test_support.rs`，55 单元测试 + 2 doctest；
五条门禁里 fmt / clippy / test / doc 全绿，`cargo deny check` 因 pingora 0.9.1 的非可选依赖
（`brotli`/`zstd`/`serde_yaml` → BSD-3-Clause、Zlib、两条 unmaintained advisory）为红，依赖策略待定，
见第 12 步。文件预算：新增文件最大 230 行。

门 A 期间还修正了一处正确性问题：pingora 的 no-provider TLS 后端是**透传** stub，`tls: true` 的 peer
会把明文发到文档声明为 `https` 的端口。`upstream::peer` 因此在该构建下直接拒绝 `scheme: https`
（`TLS_BACKEND_ENABLED` 常量 + 一条断言「日志里必须出现原因」的测试），启用 TLS 后端是独立后续任务。

## 7. `proxy.rs`：hook 顺序

- [x] `impl ProxyHttp for HttpApp`（`proxy/hooks.rs`），`type CTX = HttpContext`；`new_ctx` 空构造。
- [x] `request_filter` 按 `design.md` §4 顺序实现：trace id → 快照 → 匹配 → `Content-Length` 预检 → filter 链；
      失败一律 `respond + Ok(true)`。
- [x] `upstream_peer`：`upstream` → `upstream::peer`（先判预算）；`provider` → `ProviderNotConfigured`。
- [x] `upstream_request_filter`：`Host` 重写 + `X-Forwarded-*` + hop-by-hop 头与 `Connection` 提名头移除。
- [x] `request_body_filter`：只累计 + 越限置错并中止。
- [x] `response_filter`：`is_informational()` 直接返回（1xx 也会进此钩子）；否则 filter 链 + deadline 复核。
- [x] `upstream_response_body_filter`：deadline 复核，不改写 body。
- [x] `fail_to_proxy`：**唯一**写代理阶段失败响应的地方（`ctx.error` 优先，否则分类 Pingora 的错误）。
- [x] `error_while_proxy`：只分类（下游断开置 cancelled、清 retry 标志）——它是同步钩子，写不了响应。
- [x] `logging`：一条日志（字段见 `design.md` §11）。
- [x] 测试：`proxy/tests.rs` 覆盖 peer 预算/未知上游/过期预算、`classify` 四类、`Connection` 提名头、
      `X-Forwarded-For` 链；hook 的行为由第 11 步的集成测试覆盖。
- [x] 门禁：`cargo check -p gateway-http`。

## 8. `service.rs`：组装

- [x] `fn service(conf: &Arc<ServerConf>, snapshot: Arc<SnapshotHandle>, chain: FilterChain, settings: &HttpSettings)
      -> Service<HttpProxy<HttpApp, ()>>`：`ProxyServiceBuilder` + `server_options`（`h2c`）+ `add_tcp(&listen)`。
- [x] `fn server_conf(settings: &HttpSettings) -> Arc<ServerConf>`：`grace_period_seconds` /
      `graceful_shutdown_timeout_seconds`（向上取整且永不为 0）/ `threads` / `max_retries = 1`。
- [x] 不碰运行时与信号：`gateway-http` 只做 `ServerConf` 派生、builder 与监听；信号、`RunArgs` 与
      shutdown 桥接属于 `gateway-server/src/server/shutdown.rs`，链接本 crate 不会安装信号处理器。
- [x] 测试：向上取整、1 ms 不为 0、`max_retries == 1`、`threads` 透传、`h2c` 进入
      `HttpProxy.server_options`、监听地址注册（都不需要起服务）。
- [x] 门禁：单元测试通过。

## 9. bootstrap 的 `http` 段

- [x] `gateway-server/src/bootstrap/http.rs`：`RawHttp { listen, h2c, grace_period_ms, graceful_shutdown_ms, threads }`
      （全 `Option`，`deny_unknown_fields`），`resolve()` 把 `HttpSettings` 的错误包装成带 `http.` 前缀的
      `BootstrapError::InvalidField`；`HttpSettings` 与全部默认值/范围在 `gateway-http/src/settings.rs`。
- [x] 交叉校验（在 `HttpSettings::resolve` 里，对解析后的值）：`graceful_shutdown_ms >= grace_period_ms`；
      `listen` 非法或端口 0 给 `InvalidField`。
- [x] `config/bootstrap.yaml` 写全五个字段与注释（默认值只在 Rust 里定义一次，注释只解释含义）。
- [x] 测试：缺省段 → 全部默认（且与 `HttpSettings::default()` 相等）；装运的 YAML 等于默认；逐字段覆盖生效；
      `h2c` 缺省 false；`listen` 非法/端口 0 被拒；`graceful_shutdown_ms < grace_period_ms` 被拒；
      越界（0 / 301_000 / threads 0 / threads 257）被拒；未知字段被拒。
- [x] 门禁：`cargo test -p gateway-server`；`bootstrap.rs` 383 行（`RawHttp` 已拆到 `bootstrap/http.rs`）。

## 10. `gateway-server::run` 重新组装

- [x] `gateway-server/src/server.rs`：`serve(&BootstrapConfig, source, token) -> ExitCode`，`BootstrapConfig` →
      `server_conf` → `Server::new(None)` + 替换 `configuration` → `background_service("config-reload",
      ReloadService)` → 添加 HTTP service → `bootstrap()` → `server.run(shutdown::run_args(token))`。
- [x] `ReloadService`：`BackgroundService` 的薄适配器（`background_service` 要的是 trait 实现而非裸 future），
      用 `Mutex<Option<Reloader>>` 把管线取出后执行（锁不跨 await），失败写进共享槽并 `cancel()` token。
- [x] `CancellationToken` 的唯一来源：`run()` 创建它，`server::shutdown::CancelWatch` 桥接进 `RunArgs`，
      管线与 HTTP service 共用。
- [x] `serve()` 保持原签名与语义（管线接缝，测试在用），文档注明「生产路径由 `server::serve` 走 Pingora」。
- [x] 测试：`ReloadService` 直接驱动（失败被记录 + token 被取消；已跑过的不再启动）；`shutdown::CancelWatch`
      在 token 取消后返回 `GracefulTerminate`；真实进程的退出码由独立目标 `tests/failed_pipeline.rs` 覆盖
      （可注入 `HttpSettings`，端口由测试探得，断言起得来、停得下且退出码为 `FAILURE_EXIT_CODE`）。
- [x] 门禁：`cargo test -p gateway-server`；`lib.rs` 277 行、`server.rs` 147 行。

**门 B 记录（已完成）**：`run()` 只做组装（`server::serve`），`serve()` 只驱动管线，两者构造
`Reloader` 的参数一致，没有第二份 reload 逻辑；`gateway-http` 的 `proxy/hooks.rs` 是唯一能写代理阶段
失败响应的地方，`error.rs` 仍是唯一的 `(status, code)` 映射点。

实施中发现四处「设计初稿与 pingora 0.9.1 源码相反」的事实，已按源码修正并写回 `design.md`：

1. `ServerConf::max_retries` 数的是**尝试次数**（`pingora-proxy/src/lib.rs:1280` 的
   `while retries < max_retries`）。初稿的 `max_retries = 0` 会一次上游请求都不发就返回空响应；已改为 `1`。
2. `ServerConf::threads` 直接交给 Tokio 的 `worker_threads`，0 会 panic（`pingora-runtime` 0.9.1）；
   初稿的 `0 = 交给默认` 改为缺省 1、范围 `1..=256`。
3. `error_while_proxy` 是**同步**钩子，写不了响应；代理阶段的失败改由 `fail_to_proxy`（异步、每个最终失败
   恰好调用一次）统一写，`error_while_proxy` 只做分类。
4. `background_service` 要的是 `BackgroundService` 实现而非裸 future，且 `Server::run` 返回 `()`；
   因此有 `ReloadService` 适配器与一个失败槽来产生退出码。

另外两处实施取舍：`upstream_response_filter` 不实现（空钩子，1xx 防护移到真正改响应头的
`response_filter`）；bootstrap 的 `listen` 仍拒绝端口 0，所以第 11 步的测试服务器由测试自己探一个空闲端口
再写进配置（Pingora 不暴露实际绑定端口）。

门 B 期间做过一次临时端到端冒烟（随后删除，正式用例归第 11 步），四个场景全部通过：`/api/items` 200
（上游收到重写后的 `Host`、`X-Forwarded-*`，且没有 `Connection`）、未匹配路径 404 + 我们的 JSON
（`trace_id` 回显 `x-request-id`）、`Content-Length` 超限 413、chunked 超限 413（`request_body_filter`
中止后由 `fail_to_proxy` 写）、上游不可达 502 + `upstream_unreachable`。

## 11. 集成测试（`crates/gateway-http/tests/` 或 `gateway-server/tests/`）

- [x] `tests/support/http.rs` 及子模块：手写 tokio TCP mock 上游（脚本化响应、记录收到请求头的时刻与
      收到的头）、手写 HTTP/1.1 客户端（请求行 + 头 + 分块 body、chunked 响应解码、读响应），
      `RunningServer` 把生产装配 `server::serve` 起在 `std::thread` 上并经共享 shutdown token 停机。
      该模块按 1.1 的结论带 `#![expect(dead_code, ...)]`（每个测试目标只编译自己用到的部分）。
- [x] `tests/http_routing.rs`：最长前缀、host 优先（含更长前缀）、404、无快照 503、
      `provider` 目标 502 `provider_unavailable`、不可达上游 502 `upstream_unreachable`。
- [x] `tests/http_body.rs`：8 MiB 双向 sha256；请求分块 1/3/随机 与响应分块 1/3/997；
      不读 body（上游收到头早于首个 body 块，用两端 `Instant` 断言）。
- [x] `tests/http_streaming.rs`：SSE 200 ms 节奏与字节一致；不压缩（`Content-Encoding` 缺省）；
      不注入 `Server` 头。
- [x] `tests/http_limits.rs`：`Content-Length` 超限 413（上游未被联系）；chunked 超限 413（上游只收到
      部分 body）；上游不回答 → 504；已 committed 的流被静默切断时不写第二个响应。
- [x] `tests/http_cancel.rs`：客户端中途断开 → 上游连接关闭、日志 `error.category=Cancelled`、无 5xx、
      下一个请求仍成功。
- [x] `tests/http_shutdown.rs`：SSE 传输中取消 shutdown token，按 bootstrap 的毫秒级
      `grace_period_ms`/`graceful_shutdown_ms` 生效，`RunArgs` 与生产同路径。
- [x] `tests/http_interim.rs`：`Expect: 100-continue`，客户端收到的 100 只有上游发出的头。
- [x] 每个测试文件 ≤ 500 行；超了按场景再拆，不压缩夹具。

**第 11 步记录（已完成）**：7 个场景文件 + `tests/support/http/`（`client`、`upstream`、`source`、
`log` 与两个子模块），每个文件都远低于 500 行；测试只用手写 tokio TCP，没有 axum/hyper。

两个实测结论写回了 `design.md`：

1. **peer 超时必须是 504**。Pingora 把上游读/写/连接超时报告为 `ReadTimedout` / `WriteTimedout` /
   `ConnectTimedout` 且来源为 `Upstream`，原分类只看来源，会把不可达与超时都答成 `502`。已改为
   `classify(ctx, error)` 先判超时 → `DeadlineExceeded`（504），并补了单元测试（`proxy/tests.rs`）。
2. **`Content-Length` 响应不逐块下发**。下游 socket 是 `BufWriter`，`Content-Length` 响应只在最后一
   个字节写完时 flush，chunked 响应每个 chunk 都 flush。因此 SSE 场景的 mock 使用 chunked 帧
   （真实事件流也没有已知长度），这一约束记入 `design.md` §7/§12。

三处与计划不同，都有理由：

- 分块 1/3 的变体在 8 KiB 负载上跑，8 MiB 用粗粒度分块跑一次。1 字节分块写 8 MiB 需要 800 万次
  写，只增加运行时间，不增加覆盖（跨块状态由同一段代码承担）。
- deadline 场景改成「上游收到头后完全不回答」。Pingora 在收到上游响应头时就把响应头写进下游会话
  （`Content-Length` 时进缓冲区，chunked 时 flush），`response_written` 随之为真，此后
  `fail_to_proxy` 拒绝替换响应 —— 所以「先发头再静默」只能观察为流被切断，不能得到 504。
- `400` / `429` / `500` 的端到端用例无法在 1.2 产生：`InvalidRequest` 没有触发点（Pingora 自己拒绝的
  请求不会进本 crate），`429` 属于 1.3 的限流过滤器，`500` 需要内部不变量被破坏。这三项由
  `gateway-http/src/error/tests.rs` 的映射表单元测试覆盖，端到端覆盖的是 404/413/502/503/504 与取消日志。

## 12. 手工取证与收尾

- [x] `curl --http1.1` 与 `curl --http2-prior-knowledge` 命中同一监听；`h2c: false` 时后者失败。
      证据（命令 + 输出摘要）写入本任务 `research/h2c-curl-evidence.md`。
- [x] **依赖策略（需 owner 批准）**：pingora 0.9.1 的 `brotli`/`zstd`/`serde_yaml` 是非可选依赖，因此
      `[licenses] allow` 需追加 `BSD-3-Clause`、`Zlib`；`[advisories] ignore` 需追加
      `RUSTSEC-2024-0388`（`derivative`）、`RUSTSEC-2024-0449`（`unsafe-libyaml`，仅经
      `pingora-core → serde_yaml`，我们程序化构造 `ServerConf`，不走它的 YAML 路径）。每条都要在
      `deny.toml` 里写明依赖链与理由，不能只是静默白名单。
- [x] 五条基线：`cargo +nightly fmt --all -- --check`、`cargo clippy --workspace --all-targets
      --all-features --locked -- -D warnings`、`cargo test --workspace --all-features --locked`、
      `cargo deny check`、`RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`。
- [x] 依赖边界：`cargo tree -p gateway-core`、`cargo tree -p gateway-http | grep gateway-ai`、`cargo tree -d`。
- [x] 文件预算：`find crates -name '*.rs' | xargs wc -l | awk '$1 > 500 && $NF != "total" {print}'`
      只允许出现已存在的历史文件（`local.rs`、`reload.rs`、`cache.rs`）。
- [x] Feature 矩阵：`cargo check --no-default-features`、`cargo check --all-features`。

**核验记录（trellis-check，已完成）**：五条基线 / 文件预算 / 依赖方向 / 禁用模式 / 文档 / 错误契约 /
日志 / PRD 覆盖率 八项 PASS；「文档＝代码」一项 FAIL，已修四处陈旧表述（`design.md` §2 的
`ReloadService::new`、§12 的 body 分块与 1xx 断言位置、`implement.md` 第 1 步的 `http` 依赖与重导出清单）。

核验还报出并已处理的三处代码/需求对齐：

1. **PRD R2.2 与实现的匹配优先级不同**——PRD 写「同前缀时 host 优先」，实现是「host 档永远优先」。
   已按实现改写 PRD（host 是运维划定的信任边界，host 专属规则不应被通用长前缀抢先；与 nginx 的
   vhost-先于-location 同序），并保留原有验收条款不变。
2. **取消没有 499**——审计看板按 PRD 取 `http.status=499` 会永远为 0。日志现在对「没人可回答」的取消记
   `http.status=499` + `error.category=Cancelled`（499 从不上线；已写出响应的取消保留真实状态码）。
3. **三个无消费者的公开访问器**——`take_error` 删除；`method()` / `body_bytes()` 改为请求日志的
   `http.method` / `body_bytes` 字段（本就该记），不再是为将来预留的死 API。

保留未改（附理由）：`server/tests.rs` 里那个「真起一个 `Server::run`」的用例仍留在 lib 测试二进制
（两次运行均稳过，但它是本任务唯一的潜在 flaky 点——后续任务应把它挪到独立 `tests/` 目标）；
`http_*.rs` 每个文件只有一个大测试，是「一个测试二进制只能起一个 server」的必然结果，已在
`tests/support/http.rs` 与 `design.md` §12 记录。

## 评审门

- **门 A（第 6 步后）**：纯逻辑层（错误映射、匹配、上游、上限、filter 链）单测齐全且不依赖 Pingora 运行时；
  若此时发现某模块需要「为了测试」暴露内部状态，说明边界划错了，先改边界。
- **门 B（第 10 步后）**：`run()` 与 `serve()` 的职责没有交叉——`serve()` 只驱动管线，`run()` 只做组装；
  两者共用 `Reloader`，没有第二份 reload 逻辑。
- **门 C（第 12 步后）**：`design.md` §1 六条不变量逐条能在测试或代码里指出落点；任何一条落不了地，
  就是在实施中被悄悄放弃了。

