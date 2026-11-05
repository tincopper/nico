# 插件系统实施计划（改造现有实现）

> 设计见 `design.md`，需求见 `prd.md`。本任务的工作区已有一版实现（558 测试、五条基线绿）：它不是要丢弃的
> 代码，而是**基底**。下面的步骤按"改哪里、改到什么程度"组织，每步都要 `cargo test --workspace` 绿。

## 0. 基底与不变量

- [ ] 现有资产：`HttpState`/`chain_for_route`、`PluginRuntime::compile`、密钥 materialize、桶表复用、`Rejection`、
      401 两 code、`Identity`、`authn`、五个 `plugins_*.rs` 集成目标。保留清单见 `design.md` §9。
- [ ] 要删除的：`HOP_BY_HOP`/`connection_tokens`/16 上限/400 规则（框架已做且更严格）、`RequestHeader` 的 re-export、
      `HttpFilter` 两钩子契约、`FilterChain::run_request/run_response`。

## 1. `nico-core`：信封 + 去掉 body 上限的范围

- [x] **块名 `limits:` → `request:`**：`Limits` 更名为 `RequestBounds`（`Document::request()`），字段
      `max_request_body_bytes` → `max_body_bytes`、`request_timeout_ms` → `timeout_ms`；路由覆盖字段名不变
      （`routes[].max_body_bytes`/`timeout_ms`），两边同名让"路由 ≤ 文档"一眼可读。
- [x] 删掉 `MAX_REQUEST_BODY_BYTES` 与 `max_body_bytes` 的范围校验：写什么就是什么（`u32` 即边界，`0` 合法 =
      不接受请求体）；`timeout_ms` 保留 `1..=600_000`。保留"路由值不得大于文档值"这条收紧规则，补测试
      （`0`、`u32::MAX` 都被接受；路由值大于文档值仍被拒；`timeout_ms` 越界仍被拒）。
- [x] `config/bootstrap.yaml`：不加字节上限字段；只保留已有的数量/节点/深度 bounds。
- [x] **插件条目改成信封**：`name` + `kind: String` + `enabled: bool`（默认 true）+ `buffers_body: bool`（默认 false）
      + `error_response?` + `config: serde_norway::Value`（原样保留，core 不解释）。
- [x] **删除三个 kind 的类型**（`PluginConfig::{Jwt,ApiKey,RateLimit}` 与它们的 struct）：搬到 `nico-plugins`；
      core 的 semantic/reference 校验只保留信封与引用（重名、悬空引用、路由内重复）。`kind` 非空即合法。
- [x] 测试：信封原样往返（`config` 不被 core 改写）；未知信封字段被拒；`enabled` 缺省为 true；
      `kind: "something-new"` 在 **core 解析期就通过**（它在发布期才会因为没有工厂被拒——证明 core 不认识 kind）。
- [x] 门禁：`cargo test -p nico-core`；文件预算。

> 落地记录：`config/limits.rs` → `config/request.rs`，`Document::request()`；`nico-http` 的私有预算模块
> `limits.rs` 同步改名 `budget.rs`（`budget::{body_limit,request_budget,declared_over_limit}`）。信封实现后
> core 的 `secret_ref` 拆出 `pub fn is_secret_ref`，供 `nico-plugins` 的 `<kind>/config.rs` 复用。

## 2. `nico-http`：插件契约层（无实现）

- [x] `plugin.rs`：`Plugin`（5 阶段 + `name` + `info`，默认实现）、`Outcome`、`ResponseData`（构造校验：
      1xx/204/304 不带 body）、`PluginInfo`（`default_config: &'static str`）、`PluginError`
      （`Refused/Execute/PhaseViolation`）、`PluginContext` trait（`design.md` §2.1 的闭集）。
- [x] `plugin/registry.rs`：直接复用 `nico_core::PluginSpec`、`PluginFactory`（`kind/default_config/build`）、
      `PluginRegistry`（`register/factory`，重复注册同一 `kind` 直接错误）。
- [x] `From<PluginError> for HttpError`：`Refused` → `Rejected`，其余 → `Internal`。**唯一转换点**。
- [x] 编译期证明"契约是 WASM 可表示的"：`plugin.rs` 不出现 `pingora`/借用视图；加一条 doc 注释说明新增方法即改契约。
- [x] 测试：默认实现返回 `Continue`；`ResponseData` 的非法状态被拒；`PluginError` 的映射（三个变体各一例）。

> 落地记录（第 2 步）：`plugin.rs` 395 行（`Plugin`/`Outcome`/`PluginInfo`/`PluginContext`），
> `plugin/error.rs`（`PluginError` 3 变体 + `From<PluginError> for HttpError` 唯一转换点，另加 `LogLevel`），
> `plugin/registry.rs`（`PluginFactory::build -> Result<Arc<dyn Plugin>, PluginBuildError>`；
> `RegistryError::DuplicateKind`），`plugin/header.rs`（token/field-value 文法 + 逐跳头表，构造与清洗共用一份）。
> 与 design 的偏离：`PluginContext: Send`（`async_trait` 的 future 要 Send）；`LogLevel` 必须导出（public trait 不能
> 命名未导出类型）；`UnknownKind` 留在 `nico-plugins` 的发布期错误里，`nico-http::PluginError` 只保留三个运行期变体。

## 3. `nico-http`：数据面侧上下文

- [x] `plugin/context.rs`：`PluginContext` 实现，字段来自 `HttpContext` + 当前请求/响应头 + 当前块 + 配置文本。
- [x] 阶段校验：`set_uri` 仅 `on_request`；`request_body()` 仅 `on_request_body`；`status()` 仅响应之后；
      越界返回 `PhaseViolation`（不 panic、不静默）。
- [x] `client_ip_hash()`（64 位哈希，不暴露明文）；`set_identity` 透传（first-wins 语义保留在 `HttpContext`）。
- [x] 头操作：`set/append/remove` 各三例测试（多值与覆盖的差别、非法头名返回 `Execute`）。
- [x] 门禁：`cargo test -p nico-http`。

> 落地记录（第 3 步）：`plugin/context.rs` 429 行 + `plugin/context/tests.rs` 493 行。`Phase` 为 crate 内枚举，
> 越界访问一律 `PhaseViolation`（读取也返回 `Result`，不静默返回空值）；非 UTF-8 头值读作 `None`；
> `client_ip_hash` 先 `to_canonical()` 再 FNV-1a 64（防 IPv4-mapped IPv6 双桶），`ContextState` 不实现 `Debug`；
> `set_identity` first-wins。

## 4. `nico-http`：合成响应的清洗与写出

- [x] `plugin/response.rs`：`ResponseData` → 去逐跳头 → 忽略 `content-length` 重算 → 闭集之外不放行 →
      1xx/204/304 不带 body → 交给 `write_respond` 的唯一写出路径。
- [x] 测试：插件给 `content-length` 被忽略；插件给逐跳头被删；204 带 body 被拒；`set-cookie` 多值保留。

> 落地记录（第 4 步）：`plugin/response.rs` 250 行 + `plugin/response/tests.rs` 172 行。
> `clean_response(ResponseData) -> CleanResponse`：删固定逐跳头 + `connection` 提名的头、删 `content-length`
> （写出方按实际 body 重算）、丢 `www-authenticate`/`retry-after`（拒绝路径专用）、1xx/204/304 强制空 body；
> 其余合法 end-to-end 头按原序保留。**两条清洗规则的区别**写进了注释：拒绝走 `write_respond` 的闭集头，
> 合成响应走"合法 end-to-end + 数据面重算帧化"。
>
> 暂存说明：`context.rs`/`header.rs`/`response.rs` 带 `#![cfg_attr(not(test), expect(dead_code, reason = "..."))]`；
> 第 5 步接线后这些 `expect` 会因未触发而报错——那正是删掉它们的信号。

## 5. `nico-http`：发布值扩展与钩子重写 〔门 A〕

### 5a. 发布值（已完成）

- [x] `state.rs`：`HttpState { snapshot, policies: PublishedChains }`，`PublishedChains { global, routes, buffered }`；
      `publish(snapshot, plan)` 仍是一次原子 store；`chain_for_route` 的 fail-closed 判定保留。
- [x] `BufferedRoutes`（`BTreeSet<String>`）与 `PublishedChains` 定义在 `nico-http::state`（`nico-http` 不能依赖
      `nico-plugins`，由后者把编译结果装进这两个类型）。
- [x] `compile` 返回 `PublishedChains`：全局链、每路由链、整块路由集合一次产出；`enabled:false` 不入链但可被引用。
- [x] 编译日志按**使用点**报告 `scope`：`global` / `route` / `global+route` / `unused`（一实例可同时服务两个作用域，
      共享同一个 `Arc`）。

> **模型修正（本步发现）**：文档根 `plugins` 原本只被当作"声明表"，但 R2/R5 把它同时写成"全局作用域"——
> 于是"给 `/api/` 配一个 JWT"会顺带认证每个请求。已改成**定义与使用点分离**：
>
> - `plugins:` 只定义实例；
> - `global_plugins: [name]`（新增，默认空）是全局使用点；
> - `routes[].plugins: [name]` 仍是路由使用点。
>
> 引用校验在 `reference.rs`：名字未声明或同一列表重复 → 拒文档。默认空列表意味着只有路由插件的文档行为不变。
> PRD R2/R5 与 design §5/§6 已同步改写。

### 执行顺序：先 6a，再 5b

`Plugin` 链与 `HttpFilter` 链的元素类型不同，**钩子与插件实现必须同时换**；为了每一步都能编译、各自过门，
按下面的顺序做，而不是按编号：

1. **6a（第 6 步的前半）**：`nico-plugins` 的每个 kind 实现 `Plugin` + 工厂 + 注册表；`nico-http` 加一个
   **临时适配器** `PluginFilter: HttpFilter`（把 `on_request`/`on_response` 映射到旧两钩子；此阶段
   `Outcome::Respond` 不接入）。链里仍存 `Arc<dyn HttpFilter>`。**门 B**：401 两 code / 429 + `Retry-After` /
   桶跨 reload 全部等价。
2. **5b**：`FilterChain` 元素换成 `Arc<dyn Plugin>`，`hooks.rs` 改五阶段，删适配器与 `HttpFilter`。**门 A**。

适配器是一次性的（约百行，5b 结束即删），换来两个门分别可验；不要把它留进最终结构。

### 5b. 五阶段驱动（待做）

- [ ] `HttpFilter`/`FilterChain` → `Plugin`/`PluginChain`（五阶段 + 每阶段剩余预算）；`HttpContext` 收窄为数据面内部。
- [ ] `proxy/hooks.rs` 五阶段驱动：
      `request_filter`（快照 → 全局链 → 路由匹配 → 路由链 → 帧化守卫）→ `upstream_peer` →
      `upstream_request_filter`（整块路由强制 chunked、Host/XFF）→ `request_body_filter`（逐块或扣留）→
      `upstream_response_filter`（响应阶段：路由链 → 全局链；响应体插件路由去掉上游 CL）→
      `upstream_response_body_filter`（逐块，剩余预算复核）→ `logging`（`on_logging`：路由 → 全局）。
- [ ] 合成响应（`Outcome::Respond`）走 `clean_response` 后由 `write_respond` 写出；拒绝走 `From<PluginError>` 的唯一转换点。
- [ ] 删除 `HOP_BY_HOP`/`connection_tokens`/`MAX_CONNECTION_TOKENS` 与相关测试；**保留**一条集成断言证明逐跳头不会到上游（改为验证框架行为）。
- [ ] **门 A**：`http_*.rs` 七个目标与 `pipeline`/`local_file`/`schema_bounds`/`failed_pipeline` 全绿——数据面行为不变。

## 6. `nico-plugins`：每 kind 一个目录（配置 + 工厂 + 实现），编译器产出 `PluginPlan`

### 6a. 新接口上的三个 kind（已完成）

- [x] 每个 kind 一个目录：`jwt.rs`（`Plugin` 实现，只实现 `on_request`）+ `jwt/{config.rs,factory.rs}`（`api_key`、
      `rate_limit` 同构）。`factory.rs` 实现 `PluginFactory`（反序列化 `spec.config`、materialize 密钥、编译呈现
      模板），`plugin.rs` 的拆分留到 5b 与钩子一起做——实现与参数现在同文件，`jwt.rs` 仍在预算内。
- [x] `lib.rs` 暴露 `builtin_registry() -> PluginRegistry`：登记点只有一处，和 `PluginRuntime::new()` 同源；
      `PluginRuntime::with_registry()` 留给第 7 步的显式装配。
- [x] `compile.rs`：按注册表构建（`kind` 查工厂 → `build`）；`enabled: false` 跳过但记日志；未知 kind →
      `PluginError::UnknownKind`；`build` 失败 → `PluginError::BuildFailed`（名字段/引用 + 保留类别）；
      全局链 + 每路由链；`factory.phase()` 排序；**整块判定**；覆盖检查；intern 保留。
- [x] 临时适配器 `nico_http::PluginFilter`（`plugin/compat.rs`）：`Plugin` → `HttpFilter`，`Respond` 报
      `Internal` 并指明适配器边界；5b 连同 `HttpFilter` 一起删除。
- [x] 测试：注册表三个 kind 与各自的类别；默认配置是 kind 自己的类型接受的 JSON；`Plugin::name()` 是实例名；
      工厂对未知字段带字段路径拒绝、对读不出的凭证保留 `Upstream`；现有 `jwt`/`api_key`/`rate_limit` 用例改走
      `&mut dyn PluginContext`（测试替身 `test_support/context.rs`），断言不变。
- [x] **门 B**：`cargo test --workspace --all-features`、clippy、fmt、doc、deny 全绿。

> 落地记录（6a 的三处决定）：① `PluginFactory` 增加 `phase()`——排序归注册点，编译器不再按 kind 猜测；
> ② 桶表从 `PluginRuntime` 移到 `rate_limit` 的工厂，因为 `build(&spec)` 没有别的通道拿运行期状态，
> 记录用 `Weak` 所以表随最后一代引用消失；③ 限流键改为 `(route_id, 实例名, subject|ip_hash)`：
> `PluginContext` 只暴露 subject，取不到认证插件的 source。
>
> 连带改动：`HttpFilter::name()` 由 `&'static str` 变 `&str`，链条 Debug 现在打印**实例名**而不是 kind
> （`publication.rs` 的一条断言随之从 `["jwt"]` 改为 `["public-jwt"]`）；`PluginError::SecretRef` 增加
> `cause`（固定句子），使发布期消息能说"凭证为空/环境变量未设置"而不渲染底层错误。

### 6b. 剩余（5b 之后）

- [ ] 把 `Plugin` 实现从 `<kind>.rs` 拆到 `<kind>/plugin.rs`，让目录结构与 `spec/backend/directory-structure.md` 一致。

## 7. `nico-server`：装配与示例

- [ ] `server.rs`：构造 `PluginRegistry` 并注册三个内置工厂（这是"编译期装载"的唯一入口），把 registry 交给
      `PluginRuntime`。
- [ ] reload 发布 `PluginPlan`（与快照同一函数、无 await 间隔）。
- [ ] `config/resources.yaml`：全局插件 + 路由插件 + `buffers_body` 示例，注释写明整块模式的内存/延迟代价。
- [ ] 测试：编译失败保 LKG；`state.current()` 与快照同代；全局插件在未匹配路由的请求上也会执行。

## 8. 集成测试（`crates/nico-server/tests/`）

- [ ] `plugins_global.rs`：全局插件在 404（无路由匹配）时也执行并可拒绝；全局先于路由插件的顺序断言。
- [ ] `plugins_buffered.rs`：`buffers_body` 路由——客户端分块上传，插件看到整份并改写，上游收到改写后的字节
      （上游在客户端传完前收不到 body，用 mock 的观察时间戳断言）。
- [ ] `plugins_respond.rs`：`Outcome::Respond` 合成响应（mock 插件）；状态/头/体经清洗；`content-length` 重算。
- [ ] `plugins_auth.rs` / `plugins_limit.rs` / `plugins_custom_response.rs`：现有场景按新接口调整，断言不变。
- [ ] `plugins_streaming.rs`：SSE 路由 + `on_response_body` 插件逐块观察（不整块）；字节一致或按插件改写后的字节断言。
- [ ] 每个文件只起一个 server，≤ 500 行。

## 9. spec 与门禁

- [ ] `pingora-guidelines.md`：§2 签名（`Plugin`/`PluginContext`/`HttpState`）、§4 矩阵（合成响应、整块 413）、
      §3.1 归属表、hold-back 机制、帧化守卫、"`ServerConf` 无下游读超时"既有行保留。
- [ ] `error-handling.md`：四条呈现路径 + 清洗规则 + 4xx 边界。
- [ ] `logging-guidelines.md`：`authn`；插件日志归属（不混入请求记录）。
- [ ] `directory-structure.md`：`nico-http/plugin*`、`nico-plugins` 新模块树、可见性（只导出 `Plugin`/`PluginContext`/`Outcome`/`PluginInfo`/`PluginError`）。
- [ ] 五条基线 + feature 矩阵 + 依赖边界（`nico-core` 仍无密码学；`nico-plugins` 仍无 pingora）+ 文件预算。

## 评审门

- **门 A（第 5 步后）**：契约换了，1.2 的行为一字不差。
- **门 B（第 6 步后）**：三个内置插件在新接口上行为等价（401 两 code / 429 + `Retry-After` / 桶复用）。
- **门 C（第 8 步后）**：全局、整块、合成响应、SSE 逐块四条主线有集成证据；五条基线全绿。

## 回滚点

- 第 1 步（上限可配）：独立且可单独回退（常量恢复即可）。
- 第 5 步（五阶段钩子）：改动最集中，单独提交；回退即恢复两钩子链。
- 第 6 步（插件改接口）：只动 `nico-plugins`，与第 5 步的解耦。
