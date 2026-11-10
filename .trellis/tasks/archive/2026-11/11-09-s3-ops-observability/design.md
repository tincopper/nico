# 设计：自报面（`nico-metrics` + metrics 监听器）

## 1. 三层边界

自报面被切成三层，每层只决定一件事：

| 层 | 位置 | 决定什么 | 不决定什么 |
|---|---|---|---|
| 能力层 | `crates/nico-metrics/` | 该回什么：recorder 安装、Prometheus 渲染、就绪投影、状态行与 content-type | 监听在哪、怎么读连接、何时启动 |
| 传输层 | `crates/nico-server/src/metrics/` + `server.rs` | 怎么送到对端：端口校验与预检、Pingora service、`ServeHttp` 实现、装配顺序 | 端点语义、body 形状、指标内容 |
| 发出点 | `nico-http/src/proxy/record.rs`、`nico-server/src/reload*.rs` | 事实发生时就地记录 | 谁渲染、谁抓取 |

这样切的理由：**"一个探针该看到什么"和"一个 Pingora 进程怎么绑定端口"是两个独立的失败模式**，混在一个模块里会让二者互相拖累——前者可以脱离 socket 单测，后者需要真实进程。切面就选在这两个失败模式之间。

**能力层不依赖 Pingora，也不依赖 `nico-http`。** 它拿到的是一个 `bool`（能不能服务）加一个 `&ConfigStatus`，两者都由调用方算好。代价是就绪谓词的名字必须落在算它的那一侧（见 §4），收益是 `nico-metrics` 的测试不需要 socket、不需要异步运行时，`cargo tree -p nico-metrics` 里没有 pingora。

`.trellis/spec/backend/directory-structure.md` 的 "Module before crate" 规则要求新 crate 满足至少一条：独立 feature / 重冲突依赖 / 独立发布 / 独立团队 / 明显回本的编译隔离。本 crate 走的是**依赖隔离**这一条：把 `metrics-exporter-prometheus`（evmap / quanta / metrics-util）与"回答一个探针该回什么"的逻辑关进一个不含框架的 crate，同时让 `nico-http` 只依赖轻量的 `metrics` facade，exporter 永远不进入数据面依赖树。这条规则在 finish 阶段写进 `directory-structure.md` 的依赖图。

## 2. `nico-metrics` 契约

```rust
// 一次性安装；同进程第二次调用返回同一 handle（OnceLock），不会二次安装。
pub fn install() -> Result<Metrics, MetricsError>;

/// 自报面的一次应答：状态行、content-type、可选的 `Allow` 头与 body 文本。
pub struct Response { /* private */ }
impl Response {
    pub fn status(&self) -> u16;
    pub fn content_type(&self) -> &'static str;
    /// `405` 响应携带的 `Allow` 头取值（`Some("GET")`）；其余状态码为 `None`。
    pub fn allow(&self) -> Option<&'static str>;
    pub fn body(&self) -> &str;
}

/// 就绪事实：调用方算好的"能不能服务"，加上配置状态。
pub struct Readiness<'a> { /* private */ }
impl<'a> Readiness<'a> {
    pub fn new(servable: bool, status: &'a ConfigStatus) -> Self;
}

impl Metrics {
    /// 一个自报请求的应答。`method` 与 `path` 是请求行里的原文。
    pub fn respond(&self, method: &str, path: &str, readiness: Readiness<'_>) -> Response;
}

/// 三个端点的路径，供装配方与测试命名。
pub const LIVE_PATH: &str = "/health/live";
pub const READY_PATH: &str = "/health/ready";
pub const METRICS_PATH: &str = "/metrics";
```

设计约束：

- **`respond` 是纯同步函数**。没有 `.await`、没有锁跨越等待、没有 I/O：渲染是一次内存遍历，就绪是一次 JSON 序列化。
- **`install()` 返回 `Result`**。`PrometheusBuilder::install_recorder()` 只在全局 recorder 已被别的代码占用时失败；那是一个必须让操作员知道的启动错误（"指标面在但永远为空"是最糟的失败形态），不能静默吞掉。
- **`Response` 是值，不是 trait 对象**。能力层不定义 `Transport`/`Renderer` 之类的抽象：一个实现、一个消费者，抽象只会增加一跳。
- **`Allow` 头由能力层声明，传输层照写**：`405` 的合法方法集是这个面暴露了哪些方法的契约，只有能力层知道。传输层只写 `reply.allow()` 给出的头，自己不补 `Allow: GET`——否则面内方法集变化时，传输层那行补写就成了第二处真相。
- **路径常量在这里导出**，不在传输层复制：端点是能力层的契约。

## 3. 传输层与配置契约

```yaml
http:
  listen: 0.0.0.0:8080
metrics:                # 可选；整段缺失 = 自报面关闭
  listen: 127.0.0.1:9090
```

| 写的内容 | 结果 |
|---|---|
| 不写 `metrics` 段 | 自报面关闭（默认）。不装 recorder、不监听第二端口、日志一行 `metrics surface disabled` |
| `metrics.listen` 合法 | 自报面开启 |
| `metrics` 段存在但没有 `listen` | 启动失败，字段 `metrics.listen`，原因说明"写 `metrics` 段就必须给地址" |
| `metrics.listen` 不是合法套接字地址 | 启动失败，字段 `metrics.listen`，原因来自共用的 `validate_listen` |
| `metrics.listen` 与 `http.listen` 同址 | 启动失败，字段 `metrics.listen`，原因引用 `http.listen` 的值 |
| `metrics` 段里出现未知字段 | 启动失败（`deny_unknown_fields`，与 `RawHttp` 一致） |

不设默认端口：默认开一扇门改变所有现存部署的安全面，而"默认 9090"还会让一个只想改数据面端口的部署意外冲突。

### 装配顺序

```
serve_with()
├─ 解析 bootstrap → Option<MetricsSettings>
├─ if Some(settings):
│    ├─ 预检：std TcpListener::bind(settings.listen()) → 失败即 FAILURE_EXIT_CODE
│    ├─ registry = nico_metrics::install()?          // 失败即 FAILURE_EXIT_CODE（带原因）
│    └─ server.add_service(metrics_service(settings, snapshot, state, registry))
├─ server.add_service(config-reload background service)
├─ server.add_service(http_service(...))
└─ server.bootstrap(); server.run(run_args(shutdown))
```

三条不变量：

1. **顺序无关**：自报 service 只读两个 handle，不与数据面共享可变状态。
2. **recorder 只装一次**：`OnceLock` 在能力层内部；同进程第二次装配复用第一个 handle。
3. **共享同一个 `CancellationToken`**：Pingora 收 shutdown 时两个 service 一起停；自报面没有自己的 drain 窗口——探针在停机过程中变红是正确信号。

### 传输适配的形状

```rust
// crates/nico-server/src/metrics/service.rs
let app = HttpServer::new_app(MetricsApp { registry, snapshot, state });
let mut service = Service::new("metrics".to_string(), app);
service.add_tcp(settings.listen());
```

`Service::new` 要求 `A: ServerApp`，`HttpServer<SV>: App<HttpServer<SV>>` 对任何 `SV: ServeHttp` 成立（`pingora-core/src/apps/http_app.rs:32,46,121,137` 与 `apps/mod.rs:261`）。所以适配层只实现一个方法：

```rust
// crates/nico-server/src/metrics/app.rs
#[async_trait]
impl ServeHttp for MetricsApp {
    async fn response(&self, session: &mut ServerSession) -> pingora_http::Response<Vec<u8>> {
        let head = session.req_header();
        let (method, path) = (head.method.as_str(), head.uri.path());
        let status = self.snapshot.status();               // ConfigStatus（clone，含 digest）
        let reply = self.registry.respond(
            method,
            path,
            Readiness::new(self.state.servable(), &status),
        );
        into_pingora_response(reply)                       // 见下
    }
}
```

三处必须写对的细节：

1. **`(status, content_type, body)` 到 Pingora `Response` 的映射只有一处**（`into_pingora_response`）：`StatusCode::from_u16` 对 200/404/405/503 都会成功，但代码里不写 `expect`（clippy 禁止）：失败时归 `500` 并记一行 error。头只有三个来源：`content-type` 来自 `content_type()`，`content-length` 由 body 长度写入（绝不省略），`allow()` 为 `Some` 时写入 `Allow`——除此之外不加任何头。
2. **命名冲突要显式别名**：Pingora 的返回类型也叫 `Response`，适配层用 `use nico_metrics::Response as Reply;`，两个 `Response` 不重名。
3. **不读请求体**：`response()` 只读 `req_header()`。一个带 body 的 `GET` 也不读——读而不消费会让连接无法复用，而自报面的语义不需要 body。

这一段是本任务唯一出现在 `metrics/` 里的 Pingora 代码，且不含任何端点语义：它不知道 `/health/live` 存在，只转发方法+路径。

自报面走 h1-only：探针与 Prometheus 抓取都是 HTTP/1.1，给它开 h2c 没有消费者。

## 4. 就绪判定

**一个定义，两个消费者。** 在 `HttpStateHandle` 上新增：

```rust
/// 这个进程当前能否服务一个请求：`true` 当且仅当有一份已发布的快照。
///
/// 请求路径 `503 not_ready` 分支的谓词：该分支以 `let Some(state) = self.current() else`
/// 取快照本身，本方法是同一判定的具名形式。
pub fn servable(&self) -> bool;
```

- 请求路径：`hooks/request.rs:69` 的 `let Some(state) = app.state.current() else` **保持原样**——该分支需要的是快照本身，不只是判定结果，先调 `servable()` 再取 `current()` 反而要读两次。谓词与分支以文档互指，`503 not_ready` 的来源不变。
- 自报面：传输层调用同一个方法，把结果装进 `Readiness::new(servable, &status)`。

不引入 `ConfigStatus.ready`——它在 LKG 启动时是 `true`，而 `degraded && stale` 也为 `true`，用它做探针会在正常降级时摘流量。发布顺序还有微秒级窗口（`publish.rs:148` 写状态、`:152` 写 state handle），这正是不能用状态快照做探针的原因：窗口内的错误结论指向"还没发布"，而真实结论应当来自请求路径真正读的那一个值。窗口本身不消除——一个已经开始的请求读到旧值也是设计行为。

### `degraded` / `stale` 为什么不让 readiness 变红

这是一处刻意的判断，不是遗漏（研究笔记的约束 2 提出了相反的读法）。

- **数据面在 LKG 期间回 200，不回 503**：探针若因为 `degraded` 回 503，就会在数据面能服务时宣布它不能服务——正是「探针必须与请求路径读同一个事实」这条约束警告的谎言，只是换成另一个方向。
- **摘掉全部端点会把局部故障放大成全量故障**：配置源不可用时，k8s 把所有副本移出 endpoint，流量归零；而它们本来能继续用最后一份好配置服务。这不是保护，是放大器。
- **可表达性已经足够**：`degraded` / `stale` / `last_error_category` 出现在 `/health/ready` 的 body 里，同时是 `config_degraded` gauge。需要「配置不新鲜就别接流量」的部署可以对该 gauge 告警或做流量治理，不需要 readiness 携带第二个定义。
- **不提供 strict 开关**：一个开关就是第二套就绪语义，且目前没有任何真实部署会打开它。

### 响应形状

`/health/ready` `200`：
```json
{"ready":true,"generation":7,"revision":12,"digest":"sha256:…","stale":false,"degraded":false,
 "last_error_category":null,"last_reload_unix_secs":1791234567}
```
`503` 时同一形状，`ready:false`，其余字段来自当前 `ConfigStatus`（未知为 `null`）。`/health/live` 回 `ok`（`text/plain`），不读任何共享状态。

## 5. 指标契约

### 名字、类型与唯一写出点

| 指标 | 类型 | 标签 | 写出点（唯一） |
|---|---|---|---|
| `config_load_total` | counter | `source`, `result` | `nico-server`：`reload.rs`（每次源读取结束） |
| `config_reload_total` | counter | `result` | `nico-server`：`reload/publish.rs`（每次发布结束） |
| `config_compile_seconds` | histogram | — | `nico-server`：`reload/publish.rs` 的 `compile_document`（plugin compile + LLM compile，即"把已解析的文档变成可服务的计划"；parse 与 LKG cache flush 都在窗口之外——前者失败时根本不进入编译，后者是文件 I/O） |
| `config_watch_reconnects_total` | counter | `source` | `nico-server`：`reload/pacing.rs`（收到 `ResyncRequired`） |
| `config_snapshot_age_seconds` | gauge | — | `nico-metrics`：`/metrics` 抓取时现算 |
| `config_degraded` | gauge | — | `nico-metrics`：`/metrics` 抓取时现算 |
| `gateway_http_requests_total` | counter | `route_id`, `status_class`, `error_category` | `nico-http`：`record::emit`（每请求一次） |
| `gateway_http_request_seconds` | histogram | `route_id` | `nico-http`：`record::emit` |

前六个是 spec 已写进契约的基线（`logging-guidelines.md:239-253`），名字原样发出、不加命名空间前缀——**spec 的列表就是契约**，`gateway_<subsystem>` 模板留给本条列表之外的新指标。其中两处取值集比 spec 现文更宽，是随本任务落定的契约扩展，由 spec 改写（`implement.md` 步骤 9）收编：`status_class` 的全集定为 `1xx`–`5xx` 加 `none`（spec 现文只举 `2xx/4xx/5xx`——取值集必须完备，基数预算才可审计）；`config_watch_reconnects_total` 增加 `source` 标签，与 `config_load_total` 对齐，让一次 resync 可归因到具体配置源。

**名字归属的规则是"唯一写出点"，不是"放在一个 crate 里"。** 一个指标名字出现在它事实被决定的那一层：配置生命周期的事实只有 `nico-server` 知道，请求的事实只有 `nico-http` 知道，抓取时的两个派生值只有渲染方知道。把八个名字收进一张常量表会让三处去依赖第四处，而那条依赖边不消除任何耦合——真正防止漂移的是这张表和 spec 的那一节。

### 标签取值

- `route_id`：`ctx.route_id()`；无匹配路由用 `unmatched`（不用空串）。
- `status_class`：`written` 的 status 归 `1xx`/`2xx`/`3xx`/`4xx`/`5xx`；日志专用 `499` 归 `4xx`；`0`（什么也没写、也没取消）归 `none`。
- `error_category`：`ctx.error()` 的分类名，无错误为 `none`。
- `source`：配置源名字；`result`：`ok` / `error`。

`none` 是显式取值，不是缺失：一个没有错误分类的请求和一条标了 `error_category=""` 的记录是两件事，后者会污染按类别分组的查询。

### 抓取时 gauge

`config_snapshot_age_seconds` 与 `config_degraded` 不能只在发布时写一次——那样 age 会冻结在发布时刻。能力层在 `render()` 之前从 `ConfigStatus` 现算：

```
age      = now_unix - last_reload_unix_secs   // last_reload_unix_secs 为 None 时不写
degraded = status.degraded as u8
```

并发抓取会竞争这两个 gauge 的写入，但两值都来自同一个只读快照，竞争产生的差别不超过一次发布间隔；不引入锁。

### 抓取时 upkeep（为什么 `run_upkeep` 必须在 handler 里调）

术语：`upkeep` 是这个 crate 自己的命名（`run_upkeep`），不是软件领域的通用词——通用的说法是 housekeeping / maintenance / cleanup。本仓库只在指代这个 API 时用它，描述动作时用「归并直方图样本」「维护」这类常规词汇，避免读者需要学一个新词。

`metrics-exporter-prometheus 0.18.3` 的 `run_upkeep()` 只做两件事（docs.rs 源码视图 `recorder.rs` 的 `drain_histograms_to_distributions`）：把每个 histogram handle 里累积的样本倒进 `render()` 真正读的分布表，并刷出 help/type 描述。**不跑 upkeep，直方图渲染出来就是空的**——计数器与 gauge 直接读原子量，不受影响。

`PrometheusBuilder::install()` 起 exporter 并带周期性维护循环（默认 5s，源码视图 `exporter/builder.rs` 的 `new()`）——但它被 `http-listener` / `push-gateway` feature 门控，本任务 `default-features = false` 后根本不在依赖树里。`install_recorder()` **不起**任何后台任务、不受 feature 门控，文档原文："The caller is responsible for ensuring that upkeep is run periodically."。我们只能用后者。

因此策略是**抓取时 upkeep**：`Metrics::respond` 处理 `/metrics` 时先 `run_upkeep()` 再 `render()`。

- 不引入后台任务：没有要拥有生命周期、要负责停的 spawned task（quality-guidelines 禁止无主的 `tokio::spawn`）。
- 不浪费：维护的数据只在抓取时被读，没人抓取就不付这份成本。
- 不改语义：直方图是累计合并，早倒晚倒最终累计值一样。
- 代价是 O(直方图序列数) = O(路由数)，与 `render()` 本身同阶。

已知边界：`idle_timeout` 默认 `None`（源码视图 `exporter/builder.rs` 的 `new()`，且 `None` 会把 `recency_mask` 强制为 `NONE`，即默认不修剪任何序列）；一条序列一旦出现就在整个进程生命周期内存在。序列数被「文档里出现过的路由名总数」封顶，而不是被请求数封顶——路由名由操作者拥有，与「一条路由一个序列」是同一类基数预算。

### 热路径成本

- 记录调用点是 `metrics` 宏的字面量形式：callsite 静态，recorder 未安装时是一次原子读 + 直接返回。
- 标签值以 `SharedString` 传入（`route_id` 来自 `Arc<str>` 底层的路由 id，不重新分配）；`status_class` / `error_category` 是 `&'static str`。
- 不新增分支、不新增锁、不新增 `Instant::now()`：`elapsed_ms` 已经在 `emit` 里算好。
- 直方图单位是秒（spec 要求 `_seconds`），`elapsed_ms as f64 / 1000.0`，桶边界由 exporter 默认值提供。

## 6. 失败模式

| 失败 | 表现 | 处置 |
|---|---|---|
| `metrics.listen` 端口被占 | 预检 bind 失败 | error 日志含地址 + `FAILURE_EXIT_CODE` 退出 |
| 全局 recorder 已被占用 | `install()` 返回 `Err` | 启动失败并给出原因（不静默降级成空指标面） |
| 预检与 Pingora 真正绑定之间的 TOCTOU | Pingora `start_service` 的 `.expect` panic | 只杀自报任务，数据面继续；日志有 panic；接受并记录在案 |
| 同址配置 | 配置校验失败 | 启动失败，字段 `metrics.listen` |
| 快照尚未发布 | `/health/ready` 503，`/health/live` 200 | 探针按 k8s 语义分开使用 |
| 配置长期 degraded（LKG 服务中） | readiness 仍 200，`config_degraded=1` | 由告警策略决定，不由探针决定 |
| 同进程第二次装配 | `OnceLock` 返回既有 handle | 不失败、不重装 |
| 自报请求本身出错 | 500 | 记 error 一行；不打常规访问日志 |

## 7. 兼容性与回滚

- **默认关闭**是兼容性承诺：不写 `metrics` 段即零行为变化（不监听、不装 recorder、现有 20+ 测试夹具无需改动）。
- bootstrap 的 `metrics` 段是新增可选字段，旧文件继续合法。
- 回滚粒度：删掉 `metrics` 段即回到今天的行为；记录点留在代码里也不产生成本（无 recorder = no-op），不需要为回滚回退代码。
- 依赖面：`nico-metrics` 只新增一条 `nico-server → nico-metrics` 与 `nico-metrics → nico-core` 的边；`nico-core` / `nico-config` 依赖树不变；`nico-http` 只多一个 `metrics` facade。

## 8. 决策与被否方案

| 决策 | 否掉的方案 | 理由 |
|---|---|---|
| 能力层独立成 `nico-metrics` crate | 全部放 `nico-server/src/metrics/` | 用户决策。收益是把 exporter 与"探针该看到什么"关进不含框架的 crate，能力层可脱离 socket 单测；代价是多一个 workspace 成员与一条依赖规则 |
| 传输层留在 `nico-server` | crate 自带监听器（Pingora 进 `nico-metrics`） | 一个名叫 metrics 的 crate 里出现 HTTP 框架，会把 Pingora 升级波及它；装配 crate 已经拥有 Pingora，适配只多 30 行 |
| 监听器叫 `metrics`，不叫 `admin` | `admin.listen` | 这里没有也不会有管理操作；监控面必须无鉴权而未来的管理面必须带鉴权，共用一个端口会把两个安全等级绑死 |
| 独立监听器 | 同端口保留路径 | 一个 service 只有一个 `app_logic`；同端口意味着自报请求走完整 `HttpProxy`，`/metrics` 成为可被文档遮蔽的对象，且无法单独绑内网 |
| 默认关闭 | 默认监听 `127.0.0.1:9090` | 默认开一扇门改变所有现存部署的安全面；默认端口还会让部署意外冲突 |
| 就绪谓词具名到 `HttpStateHandle::servable()` | 两处各写 `current().is_some()` | 同一判定有两处使用后，裸表达式就会漂移；命名方法让"能不能服务"只有一处定义 |
| readiness 读 `servable()` | 读 `ConfigStatus.ready` | LKG 启动时三真，用它做探针会在正常降级时摘流量 |
| 能力层收 `servable: bool` | 让它依赖 `nico-http` 去读 handle | 依赖 `nico-http` 会把 Pingora 拉进能力层，正好否掉本 crate 的存在理由 |
| `Allow` 头由能力层 `Response::allow()` 声明 | 传输层见到 `405` 就补 `Allow: GET` | 方法集是端点契约，只有能力层知道；传输层补写会在方法集变化时留下第二处真相 |
| 指标名字留在唯一写出点 | 一个常量表 | 表没有第二消费者，且会让三处依赖第四处；契约由 spec 承载 |
| recorder 关闭时不安装 | 总是安装 | "关闭即零成本"让默认配置没有热路径开销，且不需要在记录点加分支 |
| 抓取时跑 upkeep | 后台任务每 5s 跑 | 直方图不 drain 就渲染不出来，所以 upkeep 必需；放进 handler 就不必引入 tokio、不必拥有一个后台任务，且没人抓取时不付成本 |
| 请求指标挂在 `record::emit` | 新增独立指标钩子 | `emit` 已是每请求一次的单一事实点 |
| 熔断器指标留给后续 | 本期顺带做 | 需要提升 `SharedServices` 到装配处并给 `BreakerTable` 加枚举 API |
