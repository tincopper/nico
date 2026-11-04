# gateway-http：Pingora 数据面骨架 — 技术设计

> 目标：把已发布的 `RuntimeSnapshot` 变成真实 HTTP 数据面。本文只描述最终结构；需求与验收见 `prd.md`。
> 所有 Pingora 行为结论都指向 `pingora 0.9.1` 源码（行号见各节），不是猜的。

## 1. 不变量

先写约束，再选结构。以下六条是硬约束，每个模块的存在都必须服务于其中一条：

1. **路由选择不读 body。** 阶段 0 实证：块级流式、边界不稳定，过滤器读完 body 不会重放，上游会挂起。
   因此 `request_filter` 只看请求头；body 只被**计数**，不被缓冲。
2. **单一运行时。** Pingora 拥有运行时与 HTTP I/O，不另建第二个 Tokio runtime；reload 管线作为
   `background_service` 跑在同一个运行时上。
3. **每次请求只读一次快照。** `SnapshotHandle::current()` 返回 `Arc`，请求全程持有同一个 generation；
   reload 无法在途改写。
4. **错误只映射一次。** 所有失败先变成 `gateway_http::HttpError`，再由**唯一**一处函数映射为
   `(status, code)`；任何过滤器、hook 都不自己拼响应。
5. **默认不变更上游语义。** 不开响应压缩、不加 `server` 头、不改 `Content-Length`、不重试。
6. **资源上限显式。** body 上限、连接超时、整请求 deadline、grace 宽限期全部有值且来自配置，
   不允许出现库默认（尤其 Pingora 的 300 s grace 默认值）。

## 2. 进程与运行时组合

生产只有一个入口：`gateway-server::run` → Pingora `Server`。

```text
main → gateway_server::run(args)
        ├── BootstrapConfig::load                (本地、唯一权威)
        ├── gateway_http::server_conf(&bootstrap.http())  → Arc<ServerConf> (grace / threads / retries)
        ├── Server::new(None)  → server.configuration = conf
        ├── SnapshotHandle::new()
        ├── add_service(background_service("config-reload", ReloadService { reloader, failure, shutdown: token }))
        ├── add_service(gateway_http::service(&server.configuration, snapshot, chain, settings))
        ├── server.bootstrap()
        └── server.run(server::shutdown::run_args(token))
```

依据（`pingora-core 0.9.1`）：

- `Server::run(RunArgs)` 是阻塞式的、由信号驱动；`RunArgs { shutdown_signal: Box<dyn ShutdownSignalWatch> }`
  是**可注入**的（`src/server/mod.rs:181,145`）。我们实现一个把 `CancellationToken` 映射到
  `ShutdownSignal::GracefulTerminate` 的 watch，测试因此能在同进程内触发真实停机路径，
  不必发信号、不必 fork。
- `ShutdownSignal::GracefulTerminate` 的语义是「先等 `grace_period_seconds`，再以
  `graceful_shutdown_timeout_seconds` 为上限结束 runtime」（`src/server/mod.rs:131-140`）。
  两个值都由 bootstrap 显式给出（§10）。
- 信号与停机桥接落在 `gateway-server/src/server/shutdown.rs`，不在 `gateway-http`：数据面是一个
  可嵌入的库，链接它不应该顺带安装信号处理器。`run_args(token)` 与那个 `ShutdownSignalWatch`
  实现因此是本 crate（`gateway-server`）的内部细节（`pub(super)`）。
- `services::background_service(name, task)`（`src/services/background.rs:118`）接受的是
  `BackgroundService` 实现，**不是**裸 future：因此 reload 管线外面套一个薄适配器
  （`gateway_server::server::ReloadService`）。`BackgroundService::start(&self)` 只有共享借用，
  管线只能跑一次，所以适配器用 `Mutex<Option<Reloader>>` 在第一个 `.await` 之前把管线取出来，
  同一把锁不会跨 await 持有。适配器还持有失败槽（供退出码）与取消令牌（管线放弃时停机）。
- `Server::run(RunArgs)` 返回 `()`：进程退出码由那个失败槽在 `run` 返回之后决定，而不是从
  `run` 的返回值映射。
- `ServerConf::default()` 存在（`src/server/configuration/mod.rs:219`），`server.configuration` 是
  `pub Arc<ServerConf>`（`src/server/mod.rs:224`），因此在 `server.run` 之前替换为按 bootstrap
  构造的配置。构造期读取的只有 daemon/pid 相关字段，本构建一个都不设，所以替换是完整的。

`gateway_server::serve()`（管线驱动、`CancellationToken` 可取消）保留原签名：它是**管线**的接缝，
被 `tests/pipeline.rs` 等用作单进程驱动。生产路径不再经过它，两者共用同一个 `Reloader`。

## 3. crate 结构与模块职责

`crates/gateway-http/src/`（每个文件一个边界，均远低于 500 行）：

| 文件 | 拥有 |
|---|---|
| `lib.rs` | `//!`、重导出（`HttpError`、`HttpFilter`、`HttpContext`、`Destination`、`HttpSettings`、`service`、`server_conf`、`match_route`） |
| `settings.rs` | `HttpSettings`：bootstrap `http` 段的默认值、取值范围与交叉校验（唯一来源） |
| `service.rs` | 组装：`ServerConf` 派生、`ProxyServiceBuilder`、`h2c`、监听地址（不碰运行时/信号） |
| `proxy.rs` | `HttpApp` 本身、trace id 生成、统一的拒绝路径 |
| `proxy/hooks.rs` | `impl ProxyHttp`：hook 顺序（Pingora 升级时需要复验的唯一文件） |
| `proxy/failure.rs` | 失败到线格式的落地：`classify`、`abort`、`write_respond` |
| `proxy/forward.rs` | 转发的纯逻辑：一次性的 destination 决策、`Connection` 提名头、`Host`/`X-Forwarded-*` |
| `route.rs` | `match_route(header, routes)`：host 精确 + 最长前缀，单次扫描、零分配 |
| `upstream.rs` | `Destination → HttpPeer`：TLS 能力拒绝、异步解析、peer 构造、`Host` 头规则（crate 私有，仅 `Destination` 导出） |
| `context.rs` | `HttpContext`：快照、命中的 route、destination、deadline（+`expired`/`deadline_error`）、已计 body 字节、错误槽、cancelled 标记 |
| `filter.rs` | `HttpFilter` trait + `FilterChain`（有序 `Arc<[Arc<dyn HttpFilter>]>`，1.2 为空；每个 stage 受请求预算约束） |
| `error.rs` | `HttpError` + `category()` + **唯一**的 `(status, code)` 映射与响应体 |
| `limits.rs` | body 上限判定（`Content-Length` 预检 + 累计计数）与请求预算（crate 私有） |
| `test_support.rs` | `#[cfg(test)]` 夹具（文档、路由、请求头、快照） |
| `*/tests.rs` | 各模块的单元测试（`#[cfg(test)] mod tests;`） |

依赖只有 `gateway-core` + `pingora`（`pingora-proxy`、`pingora-core`、`pingora-http`），以及
`tokio` 的 `time`/`net`/`rt`（预算计时与异步解析）——运行时、信号与停机令牌属于 `gateway-server`。
不 import `gateway-ai` / `gateway-plugins`：它们依赖本 crate，方向不能反。

## 4. 请求数据流（hook 契约）

顺序是契约。每一行都必须说明「做什么」和「为什么在这里」：

| Pingora hook | 动作 | 为什么在这里 |
|---|---|---|
| `new_ctx` | `HttpContext::default()` | 请求本地状态，不放全局 |
| `request_filter` | ①生成 trace id；②读快照；③`match_route`；④`Content-Length` 预检；⑤`FilterChain::on_request`；⑥失败即写响应并 `Ok(true)` | 阶段 0 证明此钩子在 body 之前执行；**不碰 body** |
| `upstream_peer` | ①先判剩余预算；②`destination(ctx)`：过滤器已记录者优先，否则按文档 upstream，provider 无前端则拒绝；③解析 + 构造 peer；④把 destination 记进 ctx | 决策只做一次，后面的 `Host` 重写读同一个值；响应由 `fail_to_proxy` 统一写 |
| `upstream_request_filter` | 重写 `Host`；追加 `X-Forwarded-*`；移除 hop-by-hop 头与 `Connection` 提名的头 | 上游只应看到合法的请求头 |
| `request_body_filter` | 累计字节数；超限中止（413）；预算耗尽中止（408） | 流式、不缓冲；这是唯一能在客户端「一直发」时长出请求预算的地方 |
| `response_filter` | `is_informational()` 时**直接返回**；否则 `FilterChain::on_response` + deadline 复核 | 1xx 也会进入此钩子（阶段 0 实证）；对 100 插头会破坏 `Expect: 100-continue`；此时是最后能改状态的地方 |
| `upstream_response_body_filter` | deadline 复核；不改写 body | 已 committed，只能中止流；保证「不再缓冲」不被破坏 |
| `error_while_proxy` | 只做分类：下游断开即 `ctx.mark_cancelled()`，并清空 retry 标志 | **同步钩子**，无法写响应 |
| `fail_to_proxy` | `ctx.error`（或从 Pingora 错误分类出的失败）→ **唯一**写响应处 | Pingora 对每个最终失败恰好调用一次，且默认会写框架错误页 |
| `logging` | 一条结构化请求日志 | 只在这里记，避免 filter 内重复 |

`request_filter` 的返回值语义（`src/proxy_trait.rs:102`）：已自行写完响应则 `Ok(true)`，代理就此退出；
否则 `Ok(false)` 继续。所有预检拒绝因此都收敛为「写响应 + `Ok(true)`」。

代理阶段的失败（peer 构造、连接失败、body 超限、响应期 deadline）没有各自的写响应点：它们只往
`ctx.error` 记一个 `HttpError`（或干脆返回 `Err`），由 `fail_to_proxy` 唯一地写入。`write_error_response`
在响应已提交时是 no-op（`server.rs:632`），所以「已 committed 就不写第二个响应」由 Pingora 自己保证。
`upstream_response_filter` 不实现：本任务不改写上游响应，一个空钩子只会在将来被误删，1xx 的防护放在
真正动响应头的 `response_filter` 里。

## 5. 路由匹配：单次扫描，不建第二份表

`match_route(header, &document.routes()) -> Option<&Route>`（自由函数：匹配没有状态，一个只用来当命名空间的
零大小结构体是多余的）：

- `host` 规则：route 的 `host` 为 `None` 时匹配任意 host；为 `Some(h)` 时要求请求 `Host` 与 `h` 相等
  （大小写不敏感，去掉端口）。
- 优先级：先比 host 是否具体（具体优先），再比 `path_prefix` 字节长度（长者优先）；长度相同按文档顺序。
- 复杂度：`O(routes)`，`routes ≤ max_routes`（默认 1024），全部是借用与整数比较，**零分配**。

架构文档里设想的 `CompiledRoute`（编译期排序的不可变匹配表）**本任务不实现**，理由是它要解决的
问题在这里不存在：匹配已经是类型化数据上的线性扫描，没有解析、没有分配；再加一份编译产物意味着
第二个副本、第二次失效管理（generation 变更时要重建），却换不来可测量的收益。等真实配置规模和
压测给出证据再引入，届时它落在本模块内部，不影响外部契约。`Document::routes()` 文档已声明
「文档顺序不是匹配顺序，优先级由数据面定义」，本模块就是那个定义点。

## 6. 上游选择与连接

- `upstream` 目标：先 `to_socket_addrs()` 解析（取第一个地址），再
  `HttpPeer::new(SocketAddr, tls = scheme == https, sni = host)`（`peer.rs:718`）。
  **为什么先解析**：`HttpPeer::new((host, port), ..)` 内部对解析结果 `unwrap()`（`peer.rs:719-720`），
  解析失败会 panic 在请求路径上；先解析把「解析不到」变成 `502 upstream_unreachable`。
  解析走 `tokio::net::lookup_host`（Tokio 的阻塞池），因为运行时默认只有 1 个 worker 线程，在
  worker 上做阻塞解析会让同 service 的所有请求一起等；预解析缓存留作后续任务。
- ALPN 默认 H1；PRD R3 要求 gRPC 路由显式 `ALPN::H2`，阶段 1 的 schema 没有 `grpc` 目标类型，
  因此本任务**只写默认 H1 并留注释**，不引入用不到的开关。
- **上游 TLS 在本构建里被拒绝**：`Cargo.toml` 未启用 pingora 的 TLS provider，而无 provider 时
  `noop_tls` 是**透传** stub（`src/protocols/tls/noop_tls/mod.rs` 的 `connect` 直接返回 stub stream）——
  `tls: true` 的 peer 不会加密，会把明文发到文档声明为 `https` 的端口。因此 `scheme: https` 在
  `TLS_BACKEND_ENABLED == false` 时返回 `UpstreamFailed`（502，日志带原因），绝不静默降级。
  启用 rustls 是独立任务：加一个 Cargo feature、翻一个常量、把 `https` mock 上游换成真实 TLS 监听。
- **接缝**：`upstream::Destination { scheme, host, port }` 是「请求去哪」的唯一输入，由
  target 的所有者构造：文档 upstream 由 `Destination::from(&Upstream)` 得到，provider 由
  `gateway-ai` 的过滤器解析 `base_url` 后经 `HttpContext::set_destination` 记录（1.4 接入）。
  本 crate 只负责"如何到达它"：TLS 能力、预算、`Host` 重写、hop-by-hop 清理。
- `provider` 目标：本 crate 不认识 provider 语义。若没有过滤器记录 destination，返回
  `HttpError::ProviderNotConfigured { name }` → `502 provider_unavailable`，而不是猜一个地址。
- 连接池：Pingora 的连接池是**进程级**（阶段 0 实证），因此 H1 连接数 ≈ 并发；这一事实写进
  `upstream.rs` 注释，避免后续误以为需要 per-worker 调参。
- `Host` 头重写为上游 host（默认端口省略，其余显式写出），客户端 `Host` 保留到 `X-Forwarded-Host`。
- `X-Forwarded-For` 追加客户端地址（标准语义：逗号分隔链）；`X-Forwarded-Proto` 只在缺失时写入
  `http`——把它追加成列表没有任何服务器会解析，而本构建不终止 TLS，它只能是 `http`。
- hop-by-hop 头（`Connection`/`Keep-Alive`/`TE`/`Transfer-Encoding`/`Proxy-*` 等）与请求在 `Connection`
  里提名的头一起移除。Pingora 不做这件事（它只读 `Connection` 判断 keepalive），转发提名头会把下游的
  连接语义泄漏给上游。提名列表按 16 个封顶，保证单请求工作量有界；超过上限的请求直接 `400`
  拒绝——只删一半等于把剩下的连接语义泄漏给上游，而这正是删除动作要防止的事。
- 不做 URI 归一化，路径按客户端原样转发。

## 7. 资源上限与超时

- **body 上限**：`min(route.max_body_bytes, limits.max_request_body_bytes)`。
  ① 有 `Content-Length` 时在 `request_filter` 直接判定（连接上游之前）→ `413 body_too_large`；
  ② 无 `Content-Length`（chunked）时在 `request_body_filter` 累计，越限即置 `ctx.error` 并返回 `Err`，
  由 `fail_to_proxy` 写 `413`。两条路径共用 `limits.rs` 的同一个判定函数（`over_limit`）。
- **整请求 deadline**：`now + min(route.timeout_ms, limits.request_timeout_ms)`，存在 `ctx.deadline`。
  body 与 budget 用同一条「route 只能收紧、不能放宽」规则，因此 `limits.rs` 同时提供
  `body_limit`、`request_budget` 与 `over_limit`，两个调用点共用，不各写一份 min。
  落地方式（Pingora 没有「整请求超时」旋钮）：
  - `peer.options.connection_timeout` = 剩余预算（`src/upstreams/peer.rs:502`，字段名是
    `connection_timeout`，不是 `connect_timeout`）；
  - `peer.options.read_timeout` = 剩余预算，保证慢滴上游不会永久占用；
  - `request_body_filter` 复核 deadline：body 还在到达时越界 → `408 request_timeout`（调用方慢，
    不是上游坏）。这是唯一能确定"谁慢"的位置：Pingora 没有下游读/空闲超时，`ServerConf` 0.9.1 里
    也没有这个旋钮；
  - `response_filter` 与 `upstream_response_body_filter` 复核 deadline，越界且未 committed →
    `504 upstream_timeout`；已 committed → 中止流并记 `Cancelled`（不写 5xx，字节已发出）；
  - peer 超时由 Pingora 报为 `ReadTimedout` / `WriteTimedout` / `ConnectTimedout`，此时上表两个钩子
    还没机会运行（响应头没到，或已 committed 无法替换）。`proxy/failure.rs::classify` 因此先把这三者
    归为 `DeadlineExceeded`（504）：请求自己的预算已经花完，与「peer 不可达」是两个不同的答案。
  - 边界：一个**完全停住**的上传（客户端一个字节都不再发）不会被 `request_body_filter` 再看到，
    因此由 peer 的 `read_timeout` 在同一个预算点收尾，答案是 `504`。它是"有界但不精确"的一格：
    `logging` 那行的 `body_bytes` 与 `elapsed_ms` 足以让运维区分「上游慢」和「客户端没发完」。
  - 过滤器链的每个 stage 也受同一条预算约束（`FilterChain::within_budget`），所以 1.3 的 JWT
    拉 JWKS 不可能比它服务的请求活得更久。
- **下游写出的 flush 时机（Pingora 0.9.1 实证）**：下游 socket 是 `BufWriter`，响应头只在 1xx 或
  响应**没有** `Content-Length` 时 flush；`Content-Length` 响应体只在最后一个字节写完时显式 flush
  （`v1/body.rs` 的 `do_write_body`），而 chunked 响应体每写一个 chunk 就 flush
  （`do_write_chunked_body`）。因此声明 `Content-Length` 的上游响应会按缓冲区容量攒批、直到读完才整体
  下发，chunked 响应才逐块到达。结论：SSE / 事件流必须用 chunked 帧（真实事件流本来也无从声明长度）；
  本 crate 不自行 flush，下游写出由 Pingora 拥有。
- 不设 `idle_timeout` 覆盖：默认行为已足够，改了会掩盖上游复用问题。

## 8. 过滤器扩展点（1.3 的接缝）

```rust
#[async_trait]
pub trait HttpFilter: Send + Sync {
    fn name(&self) -> &'static str;
    async fn on_request(&self, ctx: &mut HttpContext, req: &mut RequestHeader) -> Result<(), HttpError>;
    async fn on_response(&self, ctx: &mut HttpContext, resp: &mut ResponseHeader) -> Result<(), HttpError>;
}
```

- **拒绝即错误**：过滤器不写响应，返回 `Err(HttpError)`；映射点仍是 §4 的唯一一处。这样 1.3 的
  JWT/API-Key 只描述「为什么拒绝」，不复制响应拼装。
- **只开两个阶段**：请求头、响应头。body/流阶段暂不开钩子——1.4 的 SSE 转换是 AI 领域状态，
  应在自己的路径上完成，而不是把每块字节送进通用链（架构 §4.3 已排除）。
- `FilterChain` 是 `Arc<[Arc<dyn HttpFilter>]>`，顺序即执行顺序；1.2 里为空链（`&[]` 的零成本路径，
  空链不创建任何定时器）。每个 stage 用 `timeout_at(ctx.deadline())` 包住，越界即返回
  `DeadlineExceeded`：预算由链接管，过滤器不必（也不可能忘记）自己设超时。
  路由级过滤器选择要等路由 schema 有 `filters:` 字段（1.3）再引入，现在不预留字段。
- 分发成本：一次虚调用 + 一次 `.await`，相对网络 I/O 可忽略（架构 §4.3）。

`HttpContext` 是过滤器唯一能看到的世界：`trace_id`、`route_id`、`target`、`upstream`（日志名）、
`destination`（请求去哪，过滤器可写）、`config.generation`、`deadline`、`started`、`method`、`path`、
`body_limit`、`body_bytes`、`cancelled`。它不暴露 `Session`，所以过滤器无法偷偷写响应或读 body。
三个细节：

- **没有 `committed` 字段**：「响应是否已提交」由 Pingora 的 `session.response_written()` 回答，
  自建一份状态机只会与它不一致。
- **`begin(&Arc<RuntimeSnapshot>, &Route, &RequestHeader, now: Instant)`**：按值传 `Arc` 会与指向它的
  `&Route` 冲突，所以取 `&Arc`；`now` 由调用方注入，deadline 与 `elapsed_ms` 的测试因此不需要 sleep。
- **deadline 的两个派生事实归 context**：`expired(now)` 与 `deadline_error()`。hooks、转发层与过滤器链
  都问它，而不是各自重新比较 `deadline()` 并各自拼错误——同一个请求因此只有一个耗时数字。

## 9. 错误映射与响应体

`HttpError` 是唯一的失败词汇表；`fn respond(&self) -> Option<Respond>` 是唯一的映射函数，
落在 `error.rs`。`Respond { status, code, message }` 三个字段都是常量且私有，`None` 表示「响应已无法替换」
（`ResponseCommitted`）：

| `HttpError` | `ErrorCategory` | status | code |
|---|---|---|---|
| `NotReady` | `Upstream` | 503 | `not_ready` |
| `RouteNotFound` | `Client` | 404 | `route_not_found` |
| `InvalidRequest` | `Client` | 400 | `invalid_request` |
| `BodyTooLarge { limit }` | `Limit` | 413 | `body_too_large` |
| `DeadlineExceeded` | `Upstream` | 504 | `upstream_timeout` |
| `RequestTimeout` | `Client` | 408 | `request_timeout` |
| `UpstreamFailed { source }` | `Upstream` | 502 | `upstream_unreachable` |
| `ProviderNotConfigured { name }` | `Upstream` | 502 | `provider_unavailable` |
| `Internal { source }` | `Internal` | 500 | `internal_error` |
| `ResponseCommitted` | `Internal` | —（不写） | — |

响应体沿用 `error-handling.md` 的形状：`{"error":{"code","message","trace_id"}}`。`code` 与 `message`
是常量，只有调用方提供的 `trace_id` 需要 JSON 转义（它可能来自请求头，是可控输入）。`message` 只说类别
（"the request body is too large"），**不含**上游名、地址、凭证或上游 body。

`401`/`403` 不在本任务的表里：它们只在有鉴权过滤器时才可能出现，随 `1.3 gateway-plugins` 以追加变体
的方式进入（`HttpError` 是 `#[non_exhaustive]`，追加是兼容的——`Internal` 就是这样加的：Pingora 会报
出既不属于调用方也不属于上游的内部失败，500 是它唯一的结局）。

Pingora 自己报告的错误由 `proxy/failure.rs::classify` 归入同一张表，规则见 §7：peer 超时是
`DeadlineExceeded`（504），其余 `Upstream` 来源的错误是 `UpstreamFailed`（502），下游读写失败是
`Cancelled`（不写响应），没有来源的内部失败是 `Internal`（500）。`RequestTimeout` 只由
`request_body_filter` 产生——那是数据面唯一**确知**"body 还没收完"的位置。

`Cancelled` 同样不是 `HttpError` 变体：它是「不写响应、只记日志」的独立结局，由 `ctx.cancelled` 表达，
避免出现「取消也要拼一个 HTTP 响应」的诱惑。`fail_to_proxy` 因此先看 `cancelled`、再看 `ctx.error`：
一个在响应已提交后超时的请求有起因，但没有可写的响应。

## 10. bootstrap 扩展

本地 bootstrap 新增 `http` 段（仍不进资源文档：监听与停机语义属于进程，不属于被推送的配置）：

```yaml
http:
  listen: 0.0.0.0:8080
  h2c: false                  # 必须显式写；默认 false，绝不隐式开明文 H2
  grace_period_ms: 5000       # 默认 5000，显式取代 Pingora 的 300 s
  graceful_shutdown_ms: 30000 # 关闭 runtime 的上限
  threads: 1                  # 每个 service 的 worker 线程数，至少 1
```

整个 `http` 段、以及段内每个字段都可以省略；省略取默认值。规则与默认值只在
`gateway_http::HttpSettings` 里定义一次（`resolve`），bootstrap 只负责把 YAML 交给它并把失败包装成
`BootstrapError::InvalidField`（带 `http.` 前缀，`Cow` 文案引用同一个常量）：

- `listen` 必须是 `ip:port` 或 `host:port`；端口非 0。IP 字面量真正解析，域名只查形状——启动不应该
  为了一个运维自己知道的本地地址去依赖 DNS。
- `h2c` 缺省 `false`（满足「必须显式配置」）。
- `grace_period_ms` ∈ `1..=300_000`，缺省 5000；`graceful_shutdown_ms` ∈ `1..=300_000`，缺省 30000，
  且必须 ≥ 解析后的 `grace_period_ms`（交叉校验，所以单独抬高 grace 也必须同步抬 shutdown）。
- `threads` ∈ `1..=256`，缺省 1。**不能是 0**：`ServerConf::threads` 直接交给 Tokio 的
  `worker_threads`，而它在 0 上 panic（`pingora-runtime 0.9.1`），所以「交给默认」等于保留 Pingora 的 1。
- `config/bootstrap.yaml` 里逐个写明默认值与含义（运维可读），但不复制到别处。

`ServerConf` 映射：`grace_period_seconds = ceil(grace_period_ms/1000)`、`graceful_shutdown_timeout_seconds`
同式（两者都向上取整且永不为 0）、`threads` 透传、`upstream_keepalive_pool_size` 取默认。
**`max_retries = 1`**，不是 0：Pingora 的代理循环是 `while retries < max_retries`
（`pingora-proxy/src/lib.rs:1280`），它数的是「尝试次数」——写 0 会一次上游请求都不发，写 1 才是
「恰好一次、不重试」。

## 11. 观测

- **一条日志，没有 span**：Pingora 把一次请求拆成多次互不相连的 hook 调用，没有可以横跨它们的单个
  future，所以 `span.enter()` 必然跨 await（禁止）。取而代之，请求的每个字段都落在 `logging` 那一行里。
- `logging` 的级别就是运维的动作：正常请求 `debug`（禁止 `info!` 上请求路径）、`Client`/`Limit` 拒绝
  `warn`、`Upstream`/`Internal` 失败 `error`、取消（客户端断开、已 committed 后超时）`debug` 且标
  `error.category = Cancelled`。
- 取消的 `http.status` 记 `499`（nginx 惯例，也是 spec 给 `Cancelled` 的取值）：它从不上线，只进日志；
  已经写出过响应的取消保留真实状态码（例如 200），因为那时字节确实发出去了。
- 字段照 `logging-guidelines.md` 的保留表：`trace_id`、`route_id`、`upstream`、`config.generation`、
  `http.method`、`http.status`、`body_bytes`、`elapsed_ms`，失败时加 `error.category` 与 `error = %err`；
  既没写响应也没被取消时 `http.status = 0`，没有快照时 `config.generation = 0`（即 `Generation::ZERO`）。
- `trace_id` 的来源：请求带的 `x-request-id`（≤128 字节，非空）优先，否则本进程的请求序号（十六进制）。
  `gateway-core` 目前没有分布式 trace id，所以它是一个关联串，不是 trace 上下文。
- 不记 body、不记凭证、不记完整上下游 URL 的查询串（只记 path）。
- 不引入 metrics exporter。

## 12. 测试策略

不用 axum/hyper 作测试依赖（PRD 要求）。全部客户端与 mock 上游都是手写 tokio TCP：

| 层 | 手法 |
|---|---|
| 路由匹配 | 纯单元测试：拼 `Document::parse` 的 YAML，断言选中的 `route_id`（host 优先、最长前缀、404） |
| 上游选择 | 纯单元测试：`HttpPeer` 的 `addr` / `tls` / `alpn` / `Host` 重写 |
| 不读 body | mock 上游记录「收到请求头的时刻」，断言它早于下游发出第一个 body 块 |
| body 完整性 | 8 MiB 双向各一次（粗粒度分块），两端 sha256 相等；1/3/随机分块在 8 KiB 负载上验同一段跨块代码——1 字节分块写 8 MiB 是八百万次写，只增加运行时间不增加覆盖 |
| SSE 节奏 | mock 用 chunked 帧每 200 ms 发一个事件（`Content-Length` 响应在 0.9.1 下不逐块 flush，见 §7）；客户端记录到达间隔，断言事件字节与发出的一致 |
| 1xx | 客户端发 `Expect: 100-continue`；mock 上游只发一个 `x-mock-interim` 头，客户端断言收到的 100 头列表与之逐项相等（无我方添加的头），且其后的最终响应完好 |
| 取消 | 客户端写完头后断开，断言上游连接被关闭、日志为 `Cancelled`、无 5xx |
| deadline | 上游收到请求头后完全不回答，断言 `504`（peer 超时分支）；回答了一半再静默的请求已 committed，只能看到流被切断 |
| 错误映射表 | 每个变体的 status/code/category 三元组由 `error/tests.rs` 逐项覆盖；端到端覆盖本阶段有触发点的子集（404/413/502/503/504/499 日志）。400/429/500 在 1.2 没有触发路径（鉴权与限流属 1.3，内部不变量破坏不应被构造），端到端缺席是设计取舍而非遗漏 |
| body 上限 | 超限 `Content-Length` → 413（`request_filter` 预检）；chunked 越限 → 413（`request_body_filter` 中止 → `fail_to_proxy`） |
| graceful | 取消共享的 shutdown token（由 `CancelWatch` 桥接进 `RunArgs`），SSE 在传输中触发，断言 `grace_period_ms` 生效（毫秒级配置，不依赖 300 s） |
| h2c | 单元：构造出的 `HttpProxy.server_options.h2c` 与配置一致；进程级：文档化 `curl --http2-prior-knowledge` 手工取证（curl 的 H2 能力依赖构建，不能作为 CI 门的硬依赖） |

测试服务器用 `server.run(RunArgs)` 起在 `std::thread` 里。端口不能写 `127.0.0.1:0`（§10 拒绝端口 0，
而且 Pingora 不暴露实际绑定端口），所以由测试自己先绑一个临时端口再释放、把端口写进 `http.listen`；
停机一律通过取消注入的 shutdown token，与生产同一条路径（`server::shutdown::CancelWatch`）。进程级测试每个文件最多起一个 server：`Server::run`
占用调用线程并安装全局 runtime，同一个测试二进制里起第二个会测到 Pingora 的全局状态。

## 13. 风险与处置

| 风险 | 处置 |
|---|---|
| `request_filter` 里读 body 的诱惑（将来要做基于 body 的路由） | §1 不变量 + §4 表格明确「不碰 body」；需要时走独立设计（有界缓冲 + 单次重放） |
| 每请求读快照与 reload 竞争 | `ArcSwap` 单次读，请求内只持 `Arc`；generation 来自快照本身，不做二次查询 |
| Pingora 升级破坏 hook 语义 | 版本锁 `=0.9.1`；本文每条结论都带源码行号，升级时按同一份表复验 |
| `h2c` 开着但客户端发 H1 | Pingora 先 peek preface 再决定协议（`src/apps/mod.rs:270-296`），配置只影响明文 H2 是否被接受 |
| graceful 宽限期淹没停机 | 两个值都来自 bootstrap 且可配；默认 5 s / 30 s，测试用毫秒级配置验证 |
| 错误体泄漏上游细节 | 映射表只含固定文案；`UpstreamFailed` 只记日志，`source` 不进响应体 |
| 1.3 需要的东西没留缝 | `HttpFilter` 两阶段 + `Err` 拒绝语义已覆盖 JWT/API-Key/限流；路由级 filter 字段留给 1.3 的 schema 任务 |
| 本任务把 `gateway-server` 变成 Pingora 专用 | `serve()` 保留为管线接缝；`run()` 只做组装，二者共用 `Reloader`。`gateway-server` 直接依赖 `pingora`（`Server`/`RunArgs`/`background_service`），这是「只有它认识具体实现」的既有规则 |

## 14. 交付边界

- 新增 `crates/gateway-http`（依赖 `gateway-core`、`pingora`、`tokio`(signal/macros)、`tokio-util`、
  `tracing`、`thiserror`、`bytes`、`async-trait`）；改 `gateway-server`（新增 `server` 组装模块、
  `run` 改为调用它、bootstrap `http` 段、直接依赖 `pingora`）；
  改 `Cargo.toml` workspace 依赖 `pingora = "=0.9.1"`（`gateway-http` 启用 feature `proxy`）。
- `http` 类型取自 `pingora::http`（`StatusCode`/`Method`/`RequestHeader`/`ResponseHeader`），
  不额外直接依赖 `http` crate：一个协议类型只有一个版本来源。
- `cargo deny`：pingora 0.9.1 的 `brotli` / `zstd` / `serde_yaml` 是**非可选**依赖，因此引入
  `BSD-3-Clause`、`Zlib` 许可与两条 unmaintained advisory（`derivative`、`unsafe-libyaml`）。
  这是依赖策略变更，需在提交里单独说明（详见实施第 12 步的门禁记录）。
- `gateway-core` 依赖树不变（无 `pingora`）。`gateway-config` 不变。
- 不实现：过滤器业务、provider 语义、重试/fallback、TLS 终止、连接池调参、metrics、管理 API。
- Feature：本任务不新增 feature；`--no-default-features` 与 `--all-features` 都能编译（`gateway-http`
  不是可选依赖，HTTP 是阶段 1 的默认能力）。
