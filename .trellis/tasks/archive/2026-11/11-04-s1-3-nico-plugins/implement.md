# 插件系统实施计划

> 设计见 `design.md`，需求见 `prd.md`。下面的步骤按最终结构与验收门组织，每步都要 `cargo test --workspace` 绿。

## 0. 当前基线与不变量

- [x] 当前基线资产：`HttpState`/`chain_for_route`、`PluginRuntime::compile`、密钥 materialize、桶表复用、`Rejection`、
      401 两 code、`Identity`、`authn`、五个 `plugins_*.rs` 集成目标。保留约束见 `design.md` §2/§8。
- [x] 最终结构边界：逐跳头/`connection` 提名头、16 上限与 400 规则由框架处理且更严格；不导出 Pingora
      `RequestHeader`；请求路径使用 `Plugin` 五阶段驱动，不保留 `HttpFilter` 两钩子契约或两钩子链的
      `run_request/run_response` 驱动。

## 1. `nico-core`：信封与每请求资源预算

- [x] `request:` 块由 `config/request.rs` 的 `RequestBounds`（经 `Document::request()` 暴露）表示，字段
      `max_body_bytes`/`timeout_ms`；路由覆盖字段名不变
      （`routes[].max_body_bytes`/`timeout_ms`），两边同名让"路由 ≤ 文档"一眼可读。
- [x] 文档模型没有 `MAX_REQUEST_BODY_BYTES` 常量，`max_body_bytes` 只以 `u32` 的表示范围为边界
      （`0` 合法 = 不接受请求体）；`timeout_ms` 由 `RequestBounds::validate` 约束在 `1..=600_000`。
      "路由值不得大于文档值"这条收紧规则保留，补测试（`0`、`u32::MAX` 都被接受；路由值大于文档值仍被拒；
      `timeout_ms` 越界仍被拒）。
- [x] `config/bootstrap.yaml`：只声明已有的数量/节点/深度 bounds，不声明字节上限字段。
- [x] 插件条目是信封：`config/plugin.rs` 的 `name` + `kind: String` + `enabled: bool`（默认 true）+
      `buffers_body: bool`（默认 false）+ `error_response?` + `config: serde_norway::Value`（core 不解释）。
- [x] core 的 semantic/reference 校验只拥有信封与引用（重名、悬空引用、路由内重复）；`kind` 非空即合法，
      `jwt`/`api_key`/`rate_limit` 的配置结构体归 `nico-plugins` 的 `<kind>/config.rs`。`config/secret_ref.rs`
      导出 `pub fn is_secret_ref`，供这些配置模块复用。
- [x] `nico-http` 的私有预算模块是 `budget.rs`，提供 `budget::{body_limit,request_budget,declared_over_limit}`。
- [x] 测试：信封原样往返（`config` 不被 core 改写）；未知信封字段被拒；`enabled` 缺省为 true；
      `kind: "something-new"` 在 **core 解析期就通过**（它在发布期才会因为没有工厂被拒——证明 core 不认识 kind）。
- [x] 门禁：`cargo test -p nico-core`；文件预算。

## 2. `nico-http`：插件契约层（无实现）

- [x] `plugin.rs`：`Plugin`（5 阶段 + `name` + `info`，默认实现）、`Outcome`、`ResponseData`（构造校验：
      1xx/204/304 不带 body）、`PluginInfo`（`default_config: &'static str`）、`PluginContext` trait
      （`design.md` §2.1 的闭集，`Send` 使 `async_trait` 的 future 可跨线程等待）。
- [x] `plugin/error.rs`：`PluginError`（`Refused`/`Execute`/`PhaseViolation` 三个运行期变体）、`LogLevel`
      导出（public trait 可命名该类型），以及 `From<PluginError> for HttpError` 唯一转换点：`Refused` →
      `Rejected`，其余 → `Internal`。`UnknownKind` 属于 `nico-plugins` 的发布期错误。
- [x] `plugin/registry.rs`：直接复用 `nico_core::PluginSpec`，`PluginFactory`（`kind`/`default_config`/`build`，
      `build -> Result<Arc<dyn Plugin>, PluginBuildError>`）与 `PluginRegistry`（`register`/`factory`，
      重复注册同一 `kind` 返回 `RegistryError::DuplicateKind`）。
- [x] `plugin/header.rs`：token/field-value 文法与逐跳头表，构造与清洗共用一份。
- [x] 编译期证明"契约是 WASM 可表示的"：`plugin.rs` 不出现 `pingora`/借用视图；加一条 doc 注释说明新增方法即改契约。
- [x] 测试：默认实现返回 `Continue`；`ResponseData` 的非法状态被拒；`PluginError` 的映射（三个变体各一例）。

## 3. `nico-http`：数据面侧上下文

- [x] `plugin/context.rs` 与 `plugin/context/{phase,caller,tests}.rs` 按阶段集合、调用方哈希与测试分边界；
      `Phase` 是 crate 内枚举，越界访问一律 `PhaseViolation`（读取也返回 `Result`，不静默返回空值）。
- [x] 阶段校验：`set_uri` 仅 `on_request`；`request_body()` 仅 `on_request_body`；`status()` 仅响应之后；
      非 UTF-8 头值读作 `None`；`ContextState` 不实现 `Debug`。
- [x] `client_ip_hash()` 先 `to_canonical()` 再做 FNV-1a 64（避免 IPv4-mapped IPv6 产生两个桶），不暴露明文；
      `set_identity` 透传，first-wins 语义保留在 `HttpContext`。
- [x] 头操作：`set/append/remove` 各三例测试（多值与覆盖的差别、非法头名返回 `Execute`）。
- [x] 门禁：`cargo test -p nico-http`。

## 4. `nico-http`：合成响应的清洗与写出

- [x] `plugin/response.rs`：`ResponseData` → 去逐跳头 → 忽略 `content-length` 重算 → 闭集之外不放行 →
      1xx/204/304 不带 body → 交给 `write_synthetic` 的唯一写出路径（与 `write_respond` 同模块）。
- [x] `clean_response(ResponseData) -> CleanResponse` 由 `plugin/response.rs` 与 `plugin/response/tests.rs`
      覆盖：删固定逐跳头与 `connection` 提名头、忽略插件给的 `content-length`（写出方按实际 body 重算）、
      丢 `www-authenticate`/`retry-after`（拒绝路径专用）、1xx/204/304 强制空 body；其余合法 end-to-end 头
      按原序保留。拒绝路径只放行 `write_respond` 的闭集头，合成响应只放行合法 end-to-end 头并由数据面重算帧化。
- [x] 测试：插件给 `content-length` 被忽略；插件给逐跳头被删；204 带 body 被拒；`set-cookie` 多值保留。

## 5. `nico-http`：发布值与五阶段钩子 〔门 A〕

### 5a. 发布值（已完成）

- [x] `state.rs`：`HttpState { snapshot, policies: PublishedChains }`，
      `PublishedChains { global: PluginChain, routes: RouteChains, buffered: BufferedRoutes,
      rewriting_responses: ResponseRewrites }`；`publish(snapshot, chains)` 是一次原子 store；
      `chain_for_route` 的 fail-closed 判定保留。
- [x] `BufferedRoutes`（`BTreeSet<String>`）与 `PublishedChains` 定义在 `nico-http::state`（`nico-http` 不能依赖
      `nico-plugins`，由后者把编译结果装进这两个类型）。
- [x] `compile` 返回 `PublishedChains`：全局链、每路由链、整块路由集合与响应重写集合一次产出；全局链或路由链上的
      `buffers_body`/`RESPONSE_BODY` 声明分别进入 `buffered`/`rewriting_responses`；`enabled:false` 不入链但可被引用。
- [x] 编译日志按**使用点**报告 `scope`：`global` / `route` / `global+route` / `unused`（一实例可同时服务两个作用域，
      共享同一个 `Arc`）。
- [x] 文档模型按定义与使用点分离：`plugins:` 只定义实例，`global_plugins: [name]`（默认空）是全局使用点，
      `routes[].plugins: [name]` 是路由使用点；`reference.rs` 校验引用存在性与列表内唯一。空 `global_plugins`
      是默认值，仅声明而未被任何使用点引用的实例不执行。

### 接线约束

插件链元素是 `Arc<dyn Plugin>`，钩子与插件实现读取同一代 `PublishedChains`；契约层不设适配器。门 B 验证三个内置插件
在五阶段契约上行为等价（401 两 code / 429 + `Retry-After` / 桶跨 reload 复用）；门 A 验证五阶段钩子下数据面行为不变。

### 5b. 五阶段驱动（已完成）

- [x] `Plugin`/`PluginChain` 提供五阶段驱动与每阶段剩余预算复核；`HttpContext` 是数据面内部状态。
- [x] `proxy` 的五阶段驱动：
      `request_filter`（快照 → 全局链 → 路由匹配 → 路由链 → 帧化守卫）→ `upstream_peer` →
      `upstream_request_filter`（整块路由强制 chunked、Host/XFF）→ `request_body_filter`（逐块或扣留）→
      `upstream_response_filter`（响应阶段：路由链 → 全局链；响应体插件路由去掉上游 CL）→
      `upstream_response_body_filter`（逐块，剩余预算复核）→ `logging`（`on_logging`：路由 → 全局）。
- [x] 合成响应（`Outcome::Respond`）走 `clean_response` 后由 `write_synthetic` 写出（与 `write_respond` 同模块，
      唯一写出点不分裂）；拒绝走 `From<PluginError>` 的唯一转换点。
- [x] 请求方向的逐跳头、`connection` 提名头与保护字段由框架 `HttpUpstreamRequestPolicy` 处理；
      `http_body.rs` 的逐跳头断言证明该策略生效。
- [x] **门 A**：`http_*.rs` 七个目标与 `pipeline`/`local_file`/`schema_bounds`/`failed_pipeline` 全绿——数据面行为不变。

## 6. `nico-plugins`：kind 实现、工厂与 `PublishedChains` 编译

`<kind>.rs`（`Plugin` 实现）+ `<kind>/` 下的 `config`/`factory`/`json`/`tests` 等子模块，是
`directory-structure.md` 描述的 `foo.rs` + `foo/` 布局：一个 kind 是一个业务边界，配置、工厂、实现与测试分别归位。
没有 `<kind>/plugin.rs`；文件预算由门禁守卫，接近上限时才继续按边界拆分。

### 6a. 三个内置 kind（已完成）

- [x] 每个 kind 由 `<kind>.rs`（`Plugin` 实现，只实现 `on_request`）+ `<kind>/` 子模块组成：`config.rs`
      拥有该 kind 的字段与规则，`factory.rs` 反序列化 `spec.config`、materialize 密钥、编译呈现模板，
      `json.rs`/`tests.rs` 等按 kind 需要归位；`jwt`/`api_key`/`rate_limit` 同构。
- [x] `lib.rs` 暴露 `builtin_registry() -> PluginRegistry`：登记点只有一处，和 `PluginRuntime::new()` 同源；
      `PluginRuntime::with_registry()` 留给第 7 步的显式装配。
- [x] `compile.rs`：按注册表构建（`kind` 查工厂 → `build`）；`enabled: false` 跳过但记日志；未知 kind →
      `PluginError::UnknownKind`；`build` 失败 → `PluginError::BuildFailed`（名字段/引用 + 保留类别）；
      全局链 + 每路由链；`factory.phase()` 排序；全局/路由的整块与响应重写判定；覆盖检查；intern 保留。
- [x] `PluginFactory::phase()` 声明排序类别，注册点拥有顺序知识，编译器不按 `kind` 推断。
- [x] `rate_limit` 工厂持有跨 reload 的令牌桶表（`build(&spec)` 是唯一的构建通道，表用 `Weak` 记录，
      随最后一代引用消失）；限流键为 `(route_id, 实例名, subject|ip_hash)`：`PluginContext` 只暴露
      subject 与 `client_ip_hash()`，取不到认证插件的 source。
- [x] 测试：注册表三个 kind 与各自的类别；默认配置是 kind 自己的类型接受的 JSON；`Plugin::name()` 是实例名，
      链条 `Debug` 打印实例名；工厂对未知字段带字段路径拒绝、对读不出的凭证保留 `Upstream`；现有
      `jwt`/`api_key`/`rate_limit` 用例使用 `&mut dyn PluginContext`（测试替身 `test_support/context.rs`），
      断言不变。
- [x] `nico-plugins::PluginError::SecretRef` 带固定 `cause`，发布期消息可说明"凭证为空/环境变量未设置"
      而不渲染底层错误。
- [x] **门 B**：`cargo test --workspace --all-features`、clippy、fmt、doc、deny 全绿。

## 7. `nico-server`：装配与示例

- [x] `server.rs`：`serve_with(bootstrap, source, shutdown, PluginRegistry)` 是唯一的装配点，`serve` 就是
      `serve_with(..., builtin_registry())`；registry 交给 `PluginRuntime::with_registry`。注册表成为参数的两个理由写在
      doc 上：装载是组装决策（WASM 宿主将来加一个工厂），以及测试要观察阶段。`lib.rs` 的 pipeline seam 仍用
      `PluginRuntime::new()`（它等于 `with_registry(builtin_registry())`），注释说明两个入口装载同一组 kind。
- [x] reload 发布 `PublishedChains`（与快照同一函数、无 await 间隔）。
- [x] `config/resources.yaml`：全局插件 + 路由插件 + `buffers_body` 示例，注释写明整块模式的内存/延迟代价。
- [x] 测试：编译失败保 LKG；`state.current()` 与快照同代；全局插件在未匹配路由的请求上也会执行。

> 落地：装配缝是 `nico_server::server::serve_with`；测试挂具 `RunningServer::start` 委托给
> `start_with_registry`，后者把 registry 交给 `serve_with`（`start` 传 `builtin_registry()`）。`plugins_config.rs` 覆盖
> LKG、`publication.rs` 覆盖 `state.current()` 与快照同代（全局链 / 每路由链 / 整块集合都从一次 `current()` 读出），
> `plugins_global.rs` 覆盖"全局插件在 404 上执行"。`config/resources.yaml` 新增 `buffers_body: true` 的 `upload-guard`
> 与引用它的 `/upload/` 路由，`/api/` 行为不变；`nico-core` 的"随包文档可解析"用例随之更新为 3 个插件条目、3 条路由。

## 8. 集成测试（`crates/nico-server/tests/`）

- [x] `plugins_global.rs`：全局插件在 404（无路由匹配）时也执行并可拒绝；全局先于路由插件的顺序断言。
- [x] `plugins_buffered.rs`：`buffers_body` 路由——客户端分块上传，插件看到整份并改写，上游收到改写后的字节
      （上游在客户端传完前收不到 body，用 mock 的观察时间戳断言）。
- [x] `plugins_respond.rs`：`Outcome::Respond` 合成响应（mock 插件）；状态/头/体经清洗；`content-length` 重算。
- [x] `plugins_auth.rs` / `plugins_limit.rs` / `plugins_custom_response.rs`：现有场景按新接口调整，断言不变。
- [x] `plugins_streaming.rs`：SSE 路由 + `on_response_body` 插件逐块观察（不整块）；字节一致或按插件改写后的字节断言。
- [x] 每个文件只起一个 server，≤ 500 行。

> 落地：四个新目标各一个 `#[tokio::test]`、一个 server、一个文档，等待全部由事件驱动（`wait_for_status`、
> `MockUpstream::wait_for_request`、`MockRecorder::wait_for`）。`tests/support/http/mock.rs` 是只在测试注册表里的
> `mock` kind（`tag` / `respond` / `rewrite_body` / `append_response` 四个 action，配置用 `deny_unknown_fields` 解析），
> `resources.rs` 增加 `global_plugins` 与对应 builder。门 C 跑过：`cargo test -p nico-server --test plugins_global
> --test plugins_buffered --test plugins_respond --test plugins_streaming`（4 passed）、
> `cargo test --workspace --all-features`（683 passed）、clippy `-D warnings` 干净、`cargo +nightly fmt --all --check`
> 干净、`RUSTDOCFLAGS="-D warnings" cargo doc` 干净、文件预算只命中既有的 `nico-config/src/cache.rs`。
>
> 独立评审补两处：`plugins_respond.rs` 的 fixture 现在真的让插件写出 `www-authenticate`/`retry-after`，
> 否则“清洗掉拒绝专用头”只是在断言两个从未出现过的头；`request_body_filter` 与响应方向对称，同样跳过
> 零字节 body task（请求侧保留 `Some(empty)`，因为那里 `None` 表示 body 结束）。

## 9. spec 与门禁

- [x] `pingora-guidelines.md`：§2 签名（`Plugin`/`PluginContext`/`HttpState`）、§4 矩阵（合成响应、整块 413）、
      §3.1 归属表、hold-back 机制、帧化守卫、"`ServerConf` 无下游读超时"既有行保留。
- [x] `error-handling.md`：四条呈现路径 + 清洗规则 + 4xx 边界。
- [x] `logging-guidelines.md`：`authn`；插件日志归属（不混入请求记录）。
- [x] `directory-structure.md`：`nico-http/plugin*`、`nico-plugins` 新模块树、可见性（只导出 `Plugin`/`PluginContext`/`Outcome`/`PluginInfo`/`PluginError`）。
- [x] 五条基线全绿；依赖边界成立（`nico-core` 不含网络客户端依赖，也不承载策略密码学；`sha2` 仅用于
      `ConfigDigest` 的文档摘要；`nico-plugins` 不含 pingora）；文件预算成立（所有 `.rs` ≤ 500 行）。
- [x] feature 矩阵：本仓库没有任何 crate 声明 feature（`cargo metadata` 的五个 crate feature 集全为空），
      矩阵只有“默认 = 全量”一行，`--all-features` 与默认构建等价；§7 server 装配与 §8 集成测试已由门 C 覆盖。

## 评审门

- **门 A（第 5 步后）**：五阶段契约下，1.2 的行为一字不差。
- **门 B（第 6 步后）**：三个内置插件在五阶段契约上行为等价（401 两 code / 429 + `Retry-After` / 桶复用）。
- **门 C（第 8 步后）**：全局、整块、合成响应、SSE 逐块四条主线有集成证据；五条基线全绿。

## 回滚点

- 第 1 步（上限可配）：独立且可单独回退（常量恢复即可）。
- 第 5 步（五阶段钩子）：改动最集中，单独提交；回退即恢复两钩子链。
- 第 6 步（插件改接口）：只动 `nico-plugins`，与第 5 步的解耦。
