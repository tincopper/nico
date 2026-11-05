# 插件系统：一套概念、两套装载、五阶段生命周期

## Goal

让网关能按**全局 / 路由**两种作用域执行插件：**内置插件**（Rust，编进二进制）与**自定义插件**（WASM，运行时装载）
实现**同一个** `Plugin` 接口，覆盖请求头、请求体、响应头、响应体、日志五个阶段；插件可以通过配置获得上下文、
改写头部/URI/体，或者直接产出一个响应。拒绝仍是类型化的（4xx + 稳定 code + 可配置呈现）。

**命名**：`request.*` = 每请求预算，`schema_bounds.*` = 运维护栏，`kind: rate_limit` = 限流；三者各用一个词。

**核心只认信封，不认 kind**：文档里写 `{name, kind, enabled, config}`，某个 kind 的配置结构与语义由拥有它的
插件 crate 解释；`nico-core` 里没有"jwt/api_key/rate_limit"的枚举。新增一个插件不改内核。

参考模型：`https://xgpxg.github.io/aiway/gateway/plugins.html`（五阶段 / `Outcome` / `PluginContext`）与
`Bifrost`（`/Users/tomgs/workspaces/go_space/bifrost`：一套 schema、两套装载、核心只认 `BasePlugins`）。
刻意的偏离写在 R1/R4：`set_identity`、错误不一律 502、WASM 是**同步投影**。

## 背景：仓库证据（已确认）

| 事实 | 位置 / 结论 |
|---|---|
| 现有插件接口 | `nico_http::HttpFilter`：`name` + `on_request` + `on_response`，签名里带 Pingora 的 `RequestHeader` |
| 现有 kind 知识 | `nico-core` 里有闭环 enum（`PluginConfig::Jwt/ApiKey/RateLimit`）——**本次移除** |
| 现有作用域 | 只有路由级：`routes[].plugins: [name]` |
| 现有控制流 | `Result<(), HttpError>`，插件只能拒绝，不能合成响应 |
| 现有发布 | `HttpState`（快照 + 每路由链）一次发布；`PluginRuntime::compile` 编译插件、materialize 密钥、复用桶表 |
| body 上限 | 文档 `limits.max_request_body_bytes` 可配，但被语义校验限制在 `1..=8 MiB`（常量）；路由 `max_body_bytes` 只能收紧。**本次把块名改成 `request:`**（见 R5 的命名规则） |
| 其它上限 | 数量/节点/深度上限都已在 `SchemaBounds`（bootstrap `schema_bounds` 配置） |
| Pingora 钩子 | 逐跳头剥离、`Connection` 提名头剥离与保护字段拒绝、请求帧化 finalize 由框架完成（见 `pingora-guidelines.md`） |
| WASM 宿主 | 不存在；`nico-wasm` 是后续里程碑 |

## Requirements

### R1 一套接口，两套装载

- 接口是 `Plugin`：`name()`、`info()`、`on_request()`、`on_request_body()`、`on_response()`、
  `on_response_body()`、`on_logging()`，每个阶段**有默认实现**（`Ok(Outcome::Continue)`），插件只实现它用到的。
  默认实现就是 Bifrost "钩子指针为 nil = 未实现" 的 Rust 等价物。
- 插件只通过 `&mut dyn PluginContext` 访问请求/响应/配置/路由事实，**不接触 Pingora 类型**。
- 接口的数据类型必须是 **WASM 可表示**的：`String` / `Vec<u8>` / `u16` / `Vec<(String, String)>` / `bool` / `i64`；
  不得出现借用视图、Pingora 类型、`&mut Session`。
- **装载机制两套，注册表一个**：
  - `PluginFactory { kind(), default_config(), build(&PluginSpec) -> Result<Arc<dyn Plugin>, PluginError> }`；
  - **编译期（内置）**：`nico-plugins` 里的每个 kind 自带一个工厂，`nico-server` 显式注册；
  - **运行期（自定义）**：`nico-wasm` 里程碑提供一个 wasm 工厂，按模块路径装载；注册表不变；
  - 文档里的 `kind` 在注册表里查不到 → **发布期错误并指名字符串**，绝不静默跳过。
- **WASM 是同步投影**：guest 导出五个同步函数，宿主在 async 的 `Plugin` 方法里调用它（host functions 由宿主
  同步驱动并在内部受预算约束）。不要求 guest 实现 `async fn` —— 跨 FFI 轮询 guest future 不是本期要解决的问题。
- 相对 aiway 模型的一处**增加**：`PluginContext::set_identity(subject)`。没有它，认证插件（内置或 WASM）
  无法把"调用方是谁"交给限流器，按 subject 限流就只能退化成按 IP。

### R2 作用域与执行顺序

- 作用域两种：**全局**（文档根 `global_plugins: []` 列出的名字，所有请求）与**路由**（`routes[].plugins: []`）。
  模型作用域（AI provider）随 `1.4 nico-ai` 落地，走**同一个注册表、另一组 schema**。
- 请求阶段：全局 `on_request` → 路由匹配 → 路由 `on_request` → `on_request_body`。
- 响应阶段：路由 `on_response` → 全局 `on_response` → `on_response_body`。
- 日志阶段：请求结束后 `on_logging`，顺序为 路由 → 全局，且**不可中断流程**、失败不影响响应。
- 同一作用域内阶段顺序固定：认证类先于限流类；书写顺序不能反转它。
- 全局插件在路由与身份之前执行，因此全局限流只能按 IP 键；按 subject 限流必须放在路由作用域。

### R3 五个阶段与请求体模式

- `on_request`：可读写请求头、可 `set_uri`（仅此阶段生效）。
- `on_request_body`：逐块执行（默认）；**插件可以声明 `buffers_body: true`**，此时宿主把整份请求体
  聚到 `request.max_body_bytes` 再**调用一次**，插件可以整体改写；该模式按路由生效
  （该路由上任一插件声明即为整块模式），并且只允许出现在**请求**侧。
- `on_response` / `on_response_body`：读响应头/状态；响应体**永远逐块**（响应可能是无限 SSE 流），
  不允许声明整块。
- `on_logging`：只能读上下文与写日志。

### R4 控制流与错误

- 每个阶段返回 `PluginResult = Result<Outcome, PluginError>`；`Outcome::{Continue, Respond(ResponseData)}`。
- **策略拒绝走错误路径**：`Err(PluginError::Refused { .. })` → 类型化的 `HttpError`（401/403/404/413/422/429/500…），
  带稳定 code 与每实例可配置的呈现。**不采用**"插件错误一律 502"。
- **合成响应用 `Respond`**：预检、mock、缓存命中等"不是失败但也不是转发"的答案，由插件给出完整响应数据；
  数据面仍是唯一写出点，并在写之前清洗（去逐跳头、丢 `content-length` 重算、闭集头之外不放行）。
- `PluginError` 的加载/序列化/执行错误映射为 `Internal`（500），并指明插件名。

### R5 配置信封与装载

- 文档根的 `plugins: []` 是**定义表**：它声明实例，不决定在哪里运行。两个**使用点**是 `global_plugins: []`
  （全局，路由匹配之前）与既有 `routes[].plugins: []`（路由）。三处引用都必须存在、同一列表内不得重复；
  声明不等于执行 —— 一个插件只有被某个使用点列出才会运行。
- 条目是**信封**：`name` + `kind` + `enabled?`（默认 true）+ `buffers_body?` + `error_response?` + `config?`。
  `config` 的**结构由 kind 的工厂解释**（`deny_unknown_fields` 仍然生效，只是校验点从 core 移到拥有者，
  仍在发布期完成）；`nico-core` 只校验信封、名字唯一性与路由引用。
- **一个条目是一个实例**：`config` 不同即另一个条目（同一 kind 的两条路由要不同参数，就声明两个实例）；
  同一路由可以挂两个同 kind 实例（顺序仍是认证先于限流）。共享一个实例时认证/密钥共享，限流桶仍按
  `(route_id, 实例名, subject|ip)` 分开。
- `enabled: false` 的插件仍可被路由引用，但**不装载、不执行**（运营商紧急停用一条全局插件不必改所有路由）。
- **密钥只存引用**（`env:`/`file:`），在快照编译期 materialize；不可读即整份文档被拒、LKG 继续服务。
- **文档块改名为 `request:`**（原 `limits:`），字段 `max_body_bytes` 与 `timeout_ms`：

  ```yaml
  request:
    max_body_bytes: 8388608     # 每请求体上限；不设范围
    timeout_ms: 30000           # 每请求时间预算；1..=600_000
  ```

  命名规则（三条，避免"limit"一词三义）：`request.*` 是**每请求资源预算**（可被路由收紧）；
  bootstrap 的 `schema_bounds.*` 是**运维护栏**（数量/节点/深度）；`kind: rate_limit` 才是**限流限速**。
  这与 nginx 的"主语 + 资源"风格一致（`client_max_body_size` → 我们的 `request.max_body_bytes`），
  而 nginx 的 `limit_*` 专指限流，所以不把 `request` 块叫 `limits`。
- **`max_body_bytes` 不设范围**：写什么就是生效值（`u32` 的表示范围即其唯一边界），不再有 `1..=8 MiB`
  的语义校验，也不引入 bootstrap 侧的字节上限。`0` 合法，含义是"不接受任何请求体"。
  路由级 `routes[].max_body_bytes` 与 `routes[].timeout_ms` 仍**只能收紧**（内层不能放宽外层，这是护栏语义，
  与 nginx"内层覆盖外层"不同）。整块请求体的缓冲上限就是这个值——写成 1 GiB 就等于允许每个整块请求占 1 GiB，
  这是配置作者的选择，风险写在 `design.md` 的风险表里。

### R6 三个内置插件

`jwt` / `api_key` / `rate_limit` 按新接口重写：**各自的配置结构体、工厂与 `Plugin` 实现都在 `nico-plugins`**；
拒绝走 `PluginError::Refused`（401 两种 code / 429 + `Retry-After`），认证成功后 `set_identity`。
限流键仍是 `(route_id, 插件实例名, subject)`，无身份时降级为 IP 哈希；桶表有界、跨 reload 复用。

### R7 数据面契约

- 请求路径只读一个发布值 `HttpState`（快照 + 全局链 + 每路由链），一次发布。
- 唯一写出点、闭集响应头（`content-type`、`content-length`、`x-nico-error-code`、`x-nico-trace-id`，
  加 `www-authenticate`、`retry-after`）；插件给出的响应头在清洗后放行。
- 每个阶段由请求剩余预算包住（`timeout_at`）；body 阶段逐块检查预算。
- 插件链执行前快照帧化头（`content-length`/`transfer-encoding`），执行后恢复：帧化属于数据面。
- 路由声明了插件却取不到链 → `500`，绝不继续转发（fail-closed）。

### R8 可观测

- 请求日志一条（数据面），`authn` = 认证插件实例名；插件 `log()` 按插件名归属，不混入请求记录。
- 编译期：每个插件一行 `info`：`plugin`、`kind`、`scope`、`phases`、`enabled`、编译结果
  （Bifrost 的 `PluginStatus{name,status,types}` 是同一信息的运行时版本，将来状态 API 长在这上面）。
- 不记录：凭证、完整 body、明文 IP（只留哈希）。

## Acceptance Criteria

- [ ] **核心不认识 kind**：`nico-core` 里没有任何 `jwt`/`api_key`/`rate_limit` 的类型或分支；新增一个 kind 只改
      `nico-plugins` + 注册点。
- [ ] 装载：未知 `kind` → 发布期错误并指名字符串；`enabled: false` → 不执行但仍可被引用；工厂注册表是唯一入口。
- [ ] 接口：三个内置插件与新插件都只实现 `Plugin`，不出现 Pingora 类型；`PluginContext` 的类型可被 WASM 镜像
      （由一条"没有借用/没有 pingora 依赖"的依赖与可见性检查证明）。
- [ ] 顺序：全局 `on_request` 在路由匹配之前；响应与日志按 路由→全局；同作用域内认证先于限流（写反也一样）。
- [ ] 请求体：默认逐块（每块一次调用，可替换/丢弃）；声明 `buffers_body` 后整份到达、只调用一次、
      上限受 `request.max_body_bytes` 约束；响应体永不整块。
- [ ] 控制流：`Outcome::Continue` 继续；`Outcome::Respond` 合成响应（状态/头/体经清洗，`content-length` 重算）；
      `Err(Refused)` 给出类型化 4xx 与稳定 code；`Err(其它)` → 500 且日志指明插件。
- [ ] 无范围：`request.max_body_bytes` 的任何 `u32` 取值都被接受（含 `0` = 不接受请求体），
      路由 `max_body_bytes` 大于文档值仍被拒（只能收紧）。
- [ ] kind 配置：`config` 下的未知字段仍被拒（由工厂的 `deny_unknown_fields` 完成），错误指名字段路径。
- [ ] 数据面：`HttpState` 单一发布；帧化守卫（插件改 `content-length` 不生效）；缺链 fail-closed；
      每个阶段受剩余预算约束。
- [ ] 内置插件：JWT/API Key/限流的行为与现有测试等价（401 两 code、429 + `Retry-After`、桶跨 reload 复用）。
- [ ] 1.2 的七个 `http_*.rs` 集成目标与管线测试全绿（数据面行为不变）。
- [ ] 基线五条命令全绿；`nico-core` 不含密码学/网络依赖。

## Out of Scope

- **WASM 宿主与 ABI**（`nico-wasm`）：模块加载、签名、权限、沙箱预算、host functions、控制台同步。
  本任务只把"工厂注册表"这个缝留出来，并写下 WASM 的同步投影约束。
- 插件的出站 HTTP（WASM 的 `host_http_request`）：随 WASM 里程碑；内置 Rust 插件本来就能在受预算的链里发 I/O。
- 模型作用域插件（依赖 `1.4 nico-ai`）。
- RS256/JWKS/OIDC、授权（RBAC/租户）、分布式限流。
- `request.timeout_ms` 的 10 分钟上限保持常量（本次只改块名并去掉 body 上限的范围）。
- 路由级插件参数覆盖（`{name, use: {...}}`）与"按使用点关闭"（`{name, disabled: true}`）：本期只有实例级
  `enabled`；升级的触发条件与形态写在 `design.md` §5.4，业界对照见 `research/plugin-configuration-models.md`。

## Spec 交付物

- `pingora-guidelines.md`：Pingora 钩子 → 数据面/插件归属表；框架已代劳的剥离与 finalize；帧化守卫；
  body 阶段的 hold-back 机制。
- `error-handling.md`：插件拒绝的四条呈现路径与边界（4xx、闭集头、模板变量白名单）。
- `logging-guidelines.md`：`authn` 字段；插件日志的归属规则；编译期每个插件一行状态。
- `directory-structure.md`：`nico-plugins` 的新模块树（每 kind 一个目录：配置 + 工厂 + 实现）；`nico-http` 只导出
  `Plugin`/`PluginContext`/`Outcome`/`PluginFactory`/`PluginRegistry`；`nico-core` 只拥有信封。
