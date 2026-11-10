# 自报面：`nico-metrics` 能力层与独立 metrics 监听器

## Goal

给进程一个**外部可读**的自报面：k8s 探针判断何时该把流量打进来，监控系统抓取配置与请求的基线指标。这是阶段 1/2 交付后唯一不被未决语义阻塞的遗留缺口——进程里有 `ConfigStatus` 和结构化日志，但没有任何外部通路能读到它们（`crates/nico-server/src/server.rs:113` 的注释就是这句话：「this process has no readiness endpoint」）。

交付两件东西：

1. **`nico-metrics` crate（能力层）**：recorder 的一次性安装、Prometheus 文本渲染、就绪状态的投影与响应编码。**没有 HTTP 框架、没有信号、没有运行时**——它只回答"该回什么"。
2. **`nico-server` 的 metrics 监听器（传输层）**：一个独立 Pingora service，把收到的 `method` + `path` 转交给能力层，把返回值写到连接上。

## Background and Confirmed Facts

- **`ConfigStatus` 已存在但不可外读**：8 个字段（generation / revision / digest / ready / stale / degraded / last_error_category / last_reload_unix_secs），由 `SnapshotHandle` 持有（`crates/nico-core/src/status.rs:15`、`crates/nico-core/src/snapshot.rs:90`）。
- **`ready` 不等于"能服务"**：LKG 启动时 `ready && degraded && stale` 三者同真（`crates/nico-server/src/reload/startup.rs:139-142`）。请求路径真正的门是 `HttpStateHandle::current()` 为 `Some`，否则回 503 `not_ready`（`crates/nico-http/src/proxy/hooks/request.rs:69-71`）。探活语义必须与请求路径同源。
- **就绪谓词今天没有名字**：`state.current().is_some()` 裸写在请求 hook 里，第二处使用必须共享同一个定义而不是复制这个表达式。
- **一个 Pingora service 只有一个 `app_logic`**（`pingora-core-0.9.1/src/services/listening.rs:47-54`）。在数据面 service 上再 `add_tcp` 一个端口，自报请求会走完整 `ProxyHttp`，`/metrics` 变成可被配置文档路由遮蔽的对象。因此必须是独立 service。
- **新端口是全新配置面**：`RawBootstrap` / `RawHttp` 都是 `deny_unknown_fields`（`crates/nico-server/src/bootstrap.rs:218`、`crates/nico-server/src/bootstrap/http.rs:20`），`HttpSettings` 只有 `listen` 一个地址字段（`crates/nico-http/src/settings.rs:46`）。
- **Pingora 的监听器绑定失败会 panic 在 service 任务里**（`services/listening.rs` 的 `start_service` 用 `.expect("Failed to build listeners")`），仓库没有 `panic = "abort"` 的 profile，所以它只杀一个任务：进程继续服务流量但没有自报面。这是必须显式处理的失败模式。
- **spec 已预定指标方案与名字**：`.trellis/spec/backend/logging-guidelines.md:239-253` 指定 `metrics` + `nico-server` 侧的 exporter、命名与允许的标签集，并列出配置子系统基线：`config_load_total{source,result}`、`config_reload_total{result}`、`config_compile_seconds`、`config_watch_reconnects_total`、`config_snapshot_age_seconds`、`config_degraded`。
- **exporter 不会引入第二套 hyper 栈**：`metrics-exporter-prometheus 0.18.3` 的 `hyper` / `tokio` / `rustls` 都在可选 feature 后面（`http-listener` / `push-gateway`），`default-features = false` 时非可选依赖只有 base64 / evmap / indexmap / metrics / metrics-util / quanta / thiserror。两个新依赖许可证（MIT、MIT AND Apache-2.0）都在 `deny.toml` allow 列表内。
- **请求路径只有一个记录缝隙**：`record::emit`（`crates/nico-http/src/proxy/record.rs:160`）每请求恰好调用一次（调用点 `proxy/hooks/response.rs:292`），此处 status / route_id / upstream / elapsed_ms / error.category / generation 都已算好。
- **配置侧四个事件点真实存在**：load（`reload.rs`）、publish + compile（`reload/publish.rs`）、`ConfigEvent::ResyncRequired` 的接收（`reload/pacing.rs:60,70`）。

## Requirements

### R1 独立的 metrics 监听器

- bootstrap 新增可选 `metrics` 段，唯一字段 `listen`（套接字地址字符串）。**段缺失 = 自报面关闭**（默认）；段存在但 `listen` 缺失或非法 = 启动失败，不猜默认端口。
- `metrics.listen` 与 `http.listen` 解析为同一地址时启动失败（字段名下给出原因并引用 `http.listen`）。
- 独立 Pingora service + 独立监听器：不经过 `HttpProxy`、不跑插件链、不读路由文档、不做路由匹配。配置文档里的任何路径都无法遮蔽 `/health/*` 或 `/metrics`。
- 随进程同一个 `CancellationToken` 停止；没有自己的 drain 窗口。
- 不做鉴权、不做 TLS：它绑在哪里由部署决定。命名上刻意避开 `admin`——这里没有也不会有管理操作（配置推送、drain、改日志级别）；未来的管理面独立开一个 `admin` 监听器并带鉴权，两者不共享端口。

### R2 `nico-metrics` 是能力层，不是传输层

- 新建 `crates/nico-metrics/`：一次性安装 recorder、渲染 Prometheus 文本、把就绪事实投影成响应。**不依赖 Pingora、不依赖 `nico-http`、不依赖 `tokio`、不装信号、不建运行时**。
- 依赖方向：`nico-metrics → nico-core`（`ConfigStatus`）+ `metrics` + `metrics-exporter-prometheus` + `serde_json`。它不知道 `HttpStateHandle` 的存在：就绪的"能不能服务"由调用方算成一个 `bool` 传进来。
- 入口是一个同步函数：给 `method`、`path`、就绪事实，返回 `(status, content_type, body)` 与可选的 `Allow` 头（`405` 用）。单测不需要 socket，也不需要异步运行时。
- `nico-server` 持有全部传输决策：端口预检、`ServeHttp` 实现、响应写出、装配顺序。适配层是薄的：一次方法+路径转交，一次写出。

### R3 就绪谓词只有一个定义

- 在 `HttpStateHandle` 上新增具名谓词 `servable(&self) -> bool`（`true` 当且仅当有一份已发布的快照），它是「能否服务」的唯一具名定义。自报面的 `/health/ready` 直接调用它；请求路径的 `503 not_ready` 分支需要快照本身，以 `let Some(state) = current() else` 表达同一判定，两处以文档互指——这一判定不得出现第三处独立写法。

### R4 健康端点

- `GET /health/live` → `200`，`text/plain`，body `ok`。**不读任何共享状态**：进程能应答即活（读状态会让一个配置故障把进程反复重启）。
- `GET /health/ready` → `200` 当且仅当 `servable`，否则 `503`。
- body 为 JSON，字段：`ready`、`generation`、`revision`、`digest`、`stale`、`degraded`、`last_error_category`、`last_reload_unix_secs`；未知值用 `null`。
- `degraded` / `stale` 为真**不**使 readiness 失败：继续用 LKG 服务是设计行为，它们只反映在 body 和 gauge 里（详见 `design.md` §4）。
- 非 `GET` → `405` + `Allow: GET`；未知路径 → `404`。两者都不读共享状态。

### R5 指标导出

- `GET /metrics` → Prometheus 文本格式（`text/plain; version=0.0.4`）。
- 全局 recorder 在**装配期安装一次**（`OnceLock`），reload 永不重装；同进程第二次安装返回同一个 handle。
- 配置子系统指标，名字与标签沿用 spec 基线：`config_load_total{source,result}`、`config_reload_total{result}`、`config_compile_seconds`、`config_watch_reconnects_total{source}`、`config_snapshot_age_seconds`、`config_degraded`。`config_watch_reconnects_total` 在本期映射为「收到 `ConfigEvent::ResyncRequired`」——文件源没有"重连"，但该事件是同一个事实：watch 的连续性丢了、只能全量重读。
- 请求侧，在 `record::emit` 一处记录：`gateway_http_requests_total{route_id,status_class,error_category}`、`gateway_http_request_seconds{route_id}`（histogram）。
- 标签基数受控：`route_id` 取自文档（有限），未匹配路由固定 `unmatched`（不用空串）；`status_class` ∈ {`1xx`,`2xx`,`3xx`,`4xx`,`5xx`,`none`}，日志专用 `499` 归 `4xx`，什么也没写的 `0` 归 `none`；`error_category` 取分类名，无错误为 `none`。
- `config_snapshot_age_seconds` 与 `config_degraded` 是**抓取时**现算并写入的 gauge：`last_reload_unix_secs` 为 `None` 时 age 不写（缺失比 0 岁诚实）。
- 任何标签都不得携带原始路径、URL、用户标识、凭证或模型名。

### R6 关闭即零成本

自报面关闭时**不安装 recorder**。`metrics` 宏此时落到默认 no-op recorder，热路径只多一次原子读；记录调用点**不**加 `if enabled` 分支（分支会成为第二处"是否记录"的判定）。

### R7 失败与诊断

- `metrics.listen` 无法绑定：装配期预检绑定一次（bind 后立即释放），失败即按 `FAILURE_EXIT_CODE` 退出并给出地址。预检到 Pingora 真正绑定之间的 TOCTOU 窗口被接受并记录在案。
- recorder 首次安装失败（全局 recorder 已被别人占用）：启动失败并给出原因，不静默降级成"指标面存在但永远为空"。
- 装配期打印一行 info 说明状态：开启给地址，关闭明确写「metrics surface disabled」。
- 自报面的每次请求不打访问日志（探针每分钟一行没有价值）；失败路径才记 error。

### R8 角色与依赖方向

- 自报面与 `llm` feature 无关：`traffic` 角色同样提供三个端点，指标集与 `llm` 角色一致。
- 新增依赖只有 `metrics`（facade，`nico-http` 与 `nico-server` 用）、`metrics-exporter-prometheus`（只在 `nico-metrics`）。exporter 用 `default-features = false`，把 hyper / tokio / rustls 关掉，不形成第二套网络栈。
- `nico-core` 不新增依赖。`nico-http` **不得**依赖 `nico-metrics`（否则 exporter 进入数据面依赖树）；它只用 `metrics` facade 发出请求侧两条指标。
- 指标名字面量留在各自**唯一**的写出点，不建常量表。

## Out of Scope

- **熔断器 / 限流表指标**：需要把 `SharedServices` 从 `LlmRuntime::builtin()` 提到装配处、并给 `BreakerTable` 加只读枚举 API，独立可验证增量。
- 管理面（`admin` 监听器）：配置推送、drain、动态改日志级别、运行时自省。本任务只负责建立"自报面"这一扇门，且刻意与未来的管理面分开。
- 指标推送（pushgateway / OTLP）、告警规则、dashboard、SLO 定义、trace 导出。
- 自报面的鉴权、TLS、mTLS；与数据面共享端口的模式。
- 其它里程碑项不变：usage 归一化/计费、LLM token 限额、异构 Provider、QUIC、etcd。

## Acceptance Criteria

- [x] 未写 `metrics` 段时进程行为与今天完全一致：`BootstrapConfig::metrics()` 为 `None`（`bootstrap/metrics/tests.rs::an_omitted_section_disables_the_surface`），`server.rs` 因此不进装配分支——无第二监听、不装 recorder、日志写 `metrics surface disabled`；数据面上的 `/metrics` 是普通 404（`ops_surface.rs`）；全部既有夹具零改动通过。
- [x] 写了 `metrics.listen` 的进程在该地址提供三个端点；同一进程的数据面端口上，`/health/live`、`/health/ready`、`/metrics` 仍是普通路由（未声明即 404），证明自报面无法被文档遮蔽、也没有被混进数据面（`ops_surface.rs`）。
- [x] `metrics.listen` 缺失、非法、或与 `http.listen` 同址 → 启动失败且错误字段名为 `metrics.listen`（`bootstrap/metrics/tests.rs` 的三个用例：`a_present_section_without_an_address_is_rejected`、`an_illegal_address_is_rejected`、`the_data_planes_own_address_is_rejected`）。
- [x] `/health/live` 始终 200 且不依赖快照：`Readiness::live()` 不读任何参数（`response/tests.rs::live_is_always_ok_and_reads_no_state`），因此 `servable=false` 时同样应答 `ok`——快照有无不进入这条路径。
- [x] `/health/ready` 在首份快照发布前 503、发布后 200（`response/tests.rs::ready_follows_the_servability_verdict`）；发布后的 body `generation` 与在服务的代一致（`ops_surface.rs`）；`degraded && stale` 仍 ready 200 且三个字段如实出现在 body 里（`response/tests.rs::degraded_and_stale_do_not_fail_readiness`）。
- [x] 非 `GET` 请求自报面得 `405` 且响应头含 `Allow: GET`，未知路径得 `404`；两者都不读共享状态。`Allow` 由能力层 `Response::allow()` 声明（`response/tests.rs::a_non_get_request_is_refused_with_allow`）、由传输层照写（`ops_surface.rs` 断言真实响应头）。
- [x] `/metrics` 输出里能找到启动后即有声明的五个 `config_*` 指标族（`config_load_total`、`config_reload_total`、`config_compile_seconds`、`config_snapshot_age_seconds`、`config_degraded`）；`config_watch_reconnects_total` 是带标签计数器，只在收到 `ResyncRequired` 后才出现，因此新鲜进程里不可观测——这是指标语义而非缺陷。一次成功请求后出现 `gateway_http_requests_total` 的对应标签组合，其 `route_id` / `status_class` 与实际请求一致；`gateway_http_request_seconds` 存在且计数增加（`ops_surface.rs`）。
- [x] 一次失败配置发布使 `config_reload_total{result="error"}` 增加、`config_degraded` 变为 1、`/health/ready` 仍 200 且 body 报 `degraded:true`（`ops_surface.rs` 最后一个场景；已用变异检查确认该场景真会失败）；`config_snapshot_age_seconds` 随两次 scrape 递增（同文件）。
- [x] 端口被占用时进程以非零码退出：`metrics_startup.rs` 持有一个真实监听器占住 `metrics.listen`，调用生产装配入口 `serve_with`，断言退出码为 `FAILURE_EXIT_CODE` 且调用立刻返回（0.01s，即未进入 `Server::run`）；地址出现在 `AssembleError::Bind` 的 `Display` 里，由 `error = %error` 记入日志。
- [x] **能力层可脱离 HTTP 测试**：`nico-metrics` 的全部单测用 `(method, path, readiness)` 直接断言 `(status, content_type, body)`，无 socket、无 `#[tokio::test]`（`crates/nico-metrics/src/response/tests.rs`）。
- [x] **依赖边界**：`cargo tree -p nico-metrics` 不含 pingora / tokio / nico-http；`cargo tree -p nico-http` 不含 `nico-metrics` 与 `metrics-exporter-prometheus`；`nico-core` 不含 `metrics`。三条都由 `crates/nico-server/tests/dependency_boundaries.rs` 从 manifest 断言，且断言带正向校验（解析为空集时会失败而不是空过）。
- [x] 两个角色的 feature 组合都编译：`--no-default-features`、`-p nico-server --no-default-features --features traffic`、`--all-features` 三次 `cargo check` 通过；自报面与 `llm` 无耦合，`traffic` 构建里同样存在（`metrics`/`nico-metrics` 不在 feature 门内）。`cargo tree -d` 无第二套 hyper/tokio 版本。
- [x] 五条基线全绿（fmt / clippy / test / doc / deny），`--no-default-features` 可编译，全树文件 ≤ 500 行。提交时 pre-commit 钩子全量通过（1343s）。

## Open Questions

（无——范围、命名与 crate 边界已定；就绪语义与指标名字由既有 `ConfigStatus` 契约与 spec 基线固定。）
