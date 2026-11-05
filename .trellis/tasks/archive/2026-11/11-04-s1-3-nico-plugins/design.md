# 插件系统技术设计：一套接口、两种交付、五阶段

> 需求见 `prd.md`。每条结论都指向现有代码或 `pingora 0.9.1` 源码的位置。

## 1. 不变量

1. **一套接口**：内置（Rust）与自定义（WASM）插件实现同一个 `Plugin`；差异只在"谁执行"。
2. **接口只含 WASM 可表示的数据**：没有借用视图、没有 Pingora 类型、没有 `&mut Session`；上下文只通过方法访问。
3. **五阶段固定**：请求头 → 请求体 → 响应头 → 响应体 → 日志；每阶段在固定时机执行，插件不能插入新阶段。
4. **顺序不由书写决定**：同作用域内认证类先于限流类；跨作用域 请求"全局→路由"、响应/日志"路由→全局"。
5. **一个写出点**：插件产出的是**响应数据**，会话仍由 `write_respond` 唯一写；响应头集合封闭，逐跳头与帧化头由数据面重算。
6. **一个发布值**：请求路径只读 `HttpState`（快照 + 全局链 + 每路由链），快照与其派生链同时发布。
7. **配置期失败不入请求路径**：密钥、模板、范围、引用、整块模式上限都在编译期校验；失败保留 LKG。
8. **状态有界且归拥有者**：令牌桶有上限并跨 reload 复用；整块请求体的缓冲以文档里的 `request.max_body_bytes` 为界
   （该值不设范围，边界就是配置作者写的数字）。
9. **fail-closed**：路由声明了插件却取不到链 → 500；插件阶段超预算 → 按阶段语义终止（请求体阶段 408，其余按拒绝）。

## 2. 接口契约（`nico-http::plugin`）

```rust
#[async_trait]
pub trait Plugin: Send + Sync {
    fn name(&self) -> &str;              // 实例名（全局唯一），日志与限流键都用它
    fn info(&self) -> PluginInfo;        // version / description / default_config(JSON 文本) / readme / phases
    async fn on_request(&self, _ctx: &mut dyn PluginContext) -> PluginResult { Ok(Outcome::Continue) }
    async fn on_request_body(&self, _ctx: &mut dyn PluginContext) -> PluginResult { Ok(Outcome::Continue) }
    async fn on_response(&self, _ctx: &mut dyn PluginContext) -> PluginResult { Ok(Outcome::Continue) }
    // 唯一的同步阶段：Pingora 的响应体钩子是同步的（`upstream_response_body_filter`），
    // async fn 无法在其中 await。WASM 宿主本来也是同步投影，所以这条约束不反过来改接口。
    fn on_response_body(&self, _ctx: &mut dyn PluginContext) -> PluginResult { Ok(Outcome::Continue) }
    async fn on_logging(&self, _ctx: &mut dyn PluginContext) {}
}

pub type PluginResult = Result<Outcome, PluginError>;

pub enum Outcome {
    Continue,
    Respond(ResponseData),   // 预检 / mock / 缓存命中：不是失败，也不是转发
}

pub struct ResponseData {  // 字段私有、构造校验
    status: u16,                     // 100..=599，但 1xx 与 204/304 不允许带 body
    headers: Vec<(String, String)>,  // 由数据面清洗后放行
    body: Vec<u8>,
}

pub enum PluginError {
    Refused { rejection: Arc<Rejection>, detail: &'static str, retry_after: Option<Duration> },
    Execute(&'static str),           // 插件业务失败 → 500，日志指 name
    PhaseViolation(&'static str),    // 在该阶段非法的方法调用 → 500（插件 bug，不是调用方的错）
}
```

- **`PluginError` 取代 `HttpError` 作为插件的失败类型**：插件不该知道 `HttpError` 的完整词汇（上游/超时/取消都与它无关）。
  `From<PluginError> for HttpError` 是唯一转换点：`Refused` → `Rejected`，其余 → `Internal`。
- `PluginInfo::default_config` 是 `&'static str`（JSON 文本），避免 `nico-http` 引入 `serde_json`。
- 阶段有默认实现，内置插件只实现它用到的；WASM 宿主为缺省阶段导出一个返回 `Continue` 的桩。
- `PluginInfo::phases: PluginPhases` 是**声明**，不是开关：驱动仍然调用每个阶段。它只决定一件数据面
  无法事后推断的事——**帧化**。链上若有插件声明 `RESPONSE_BODY`，该路由的响应就不能保留上游
  `content-length`（插件替换或丢弃块后它描述的字节已经不存在），因此发布期把它记进
  `ResponseRewrites`，响应头阶段删掉 CL 让 Pingora 改写 chunked。少声明会让长度与实际字节脱节，
  多声明只是多一次 chunked——两个方向的代价不同，契约里写清。

### 2.1 上下文（`PluginContext`）

| 组 | 方法（读写语义按阶段受限） |
|---|---|
| 请求事实 | `trace_id()`、`method()`、`uri()`、`set_uri()`（仅 `on_request`）、`is_sse()`、`is_websocket()` |
| 请求头 | `request_header(name)`、`set_request_header`、`append_request_header`、`remove_request_header` |
| 响应头/状态 | `status()`、`response_header(name)`、`set/append/remove_response_header` |
| body | `request_body()` / `set_request_body()`（`on_request_body`）、`response_body()` / `set_response_body()`（`on_response_body`）、`body_complete()` |
| 路由与配置 | `route_id()`、`target()`（upstream/provider 名）、`config()`（JSON 文本） |
| 身份与来源 | `set_identity(subject)`、`identity()`、`client_ip_hash()` |
| 插件自身 | `plugin_name()`、`log(level, msg)` |

三条约束：

- **闭集**：加一个方法就是改契约（WASM 侧要同步实现），所以清单要短。
- **阶段外调用返回 `PhaseViolation`**，不 panic、不静默无效：`set_uri` 只在 `on_request`，`request_body()` 只在
  `on_request_body`，`status()` 只在响应阶段之后。请求头**读**在两个请求阶段都可用，**写**只在 `on_request`
  —— 上游头在 `upstream_request_filter` 就发出去了，在 body 阶段写它永远不会到达对端。响应头同理只在
  `on_response` 可写：之后客户端手里已经有那个头。
- **帧化头不可写**：`set/append/remove_*_header` 对 `content-length` 与 `transfer-encoding` 返回 `Execute`。
  头必须描述真正的字节，而只有数据面知道那些字节；请求侧另有快照/恢复守卫，合成响应会重算长度。
- `client_ip_hash()` 而不是明文 IP：限流键只需要哈希，WASM 侧不该拿到 PII。

### 2.2 装载：两套来源，一个注册表

文档里的 `kind` 是一个**字符串**，它到运行期实现的映射只存在于注册表：

```rust
// nico-http：装载缝。内置插件与（后续）WASM 插件都从这里进入，核心不知道来源。
pub struct PluginSpec { /* name, kind, enabled, buffers_body, error_response, config(JSON 文本) */ }

pub trait PluginFactory: Send + Sync {
    fn kind(&self) -> &'static str;                 // "jwt" / "api_key" / "rate_limit" / 将来的 "wasm"
    fn phase(&self) -> PluginPhase;                 // Authenticate | Limit | Other：同作用域内的执行序
    fn default_config(&self) -> &'static str;       // 文档与 `PluginInfo` 共用
    fn build(&self, spec: &PluginSpec) -> Result<Arc<dyn Plugin>, PluginBuildError>;
}

pub struct PluginRegistry { /* kind -> Arc<dyn PluginFactory> */ }
impl PluginRegistry {
    pub fn register(&mut self, factory: Arc<dyn PluginFactory>);
    pub fn factory(&self, kind: &str) -> Option<&Arc<dyn PluginFactory>>;
}
```

- **编译期（内置）**：`nico-plugins` 为每个 kind 提供一个工厂（`jwt::JwtFactory` 等），由
  `nico_plugins::builtin_registry()` 集合，`nico-server` 在组装时把它交给 `PluginRuntime`。工厂拥有该 kind 的
  **配置结构体**：`build` 里用 `deny_unknown_fields` 反序列化 `spec.config`，所以"未知字段被拒"没有丢，
  只是校验点从 `nico-core` 移到拥有者，仍然发生在发布期。
- **`phase()` 归注册点，不归编译器**：认证先于限流是 kind 的知识，编译器只按 `PluginPhase` 排序，不再
  匹配 `kind` 字符串。变体顺序即执行序（`Authenticate` → `Limit` → `Other`），同一类内按文档序；WASM 工厂
  将来用同一个方法声明自己的类别。方法无默认实现：一个默默落到"最后"的 kind 会被排在必须先行者之后。
- **`build` 的错误类型是 `PluginBuildError`**（`nico-http`）：消息（指名字段或引用，绝不引用凭证值）与
  `ErrorCategory` 以数据形式跨过装载缝，因为 `nico-http` 不能命名拥有 kind 的 crate 的错误类型。编译器把它
  变成 `PluginError::BuildFailed`，保留类别，所以"凭证读不出来"仍报 `Upstream` 而不是文档笔误。
- **跨 reload 的状态归工厂**：发布只给 `build` 一个文档条目，别处没有通道传入运行时状态。因此令牌桶表由
  `rate_limit` 的工厂持有（按实例名 + 参数复用，用 `Weak` 记录，表随最后一代引用消失），而不是
  `PluginRuntime` 的一个字段。
- **运行期（自定义）**：`nico-wasm` 里程碑注册一个 wasm 工厂（按模块路径/URL 装载并包成 `Arc<dyn Plugin>`）；
  注册表、链、阶段、`Outcome` 全部不变。
- **未知 kind**：`compile` 查不到工厂 → `PluginError::UnknownKind { kind }`，整份文档被拒、LKG 继续服务。
  绝不静默跳过（跳过就是 fail-open）。
- **`enabled: false`**：工厂不构建、链里不出现，但名字仍可被路由引用（引用校验只看声明是否存在）。编译日志
  仍打一行，状态是 `disabled`。

**WASM 的形态（写进契约，本期不实现）**：guest 导出五个**同步**函数；宿主在 async 的 `Plugin` 方法里调用它，
host functions 由宿主同步驱动并在内部受请求预算约束。guest 不实现 `async fn` —— 跨 FFI 轮询 guest future
不在本期范围。trait 的数据类型已经是 owned/WASM 可表示的（§1 不变量 2），所以这条约束不会反过来改接口。

## 3. 作用域、阶段与 Pingora 钩子

| 阶段 | 全局插件 | 路由插件 | Pingora 钩子（调用点） |
|---|---|---|---|
| `on_request` | ✅ 在**路由匹配之前** | ✅ 在路由匹配之后 | `request_filter`（`lib.rs:1189`）——全局链读快照后立刻跑，再匹配路由 |
| `on_request_body` | ✅ | ✅ | `request_body_filter`（`proxy_h1.rs:1045`，逐块） |
| `on_response` | ✅ | ✅（先） | `response_filter` 之前的 `upstream_response_filter`（`lib.rs:500`）——路由链先、全局链后 |
| `on_response_body` | ✅ | ✅（先） | `upstream_response_body_filter`（`lib.rs:506`，逐块、**同步**） |
| `on_logging` | ✅（最后） | ✅（先） | `logging`（`lib.rs:533/1387`） |

规则：

- **同作用域内阶段顺序固定**：认证类（jwt/api_key）先于限流类。书写顺序不改变它（`compile` 排序，不是运行期判断）。
- **全局在路由之前**，所以全局阶段没有 route id、没有身份：全局限流只能按 `client_ip_hash` 键，按 subject 限流的插件必须放路由作用域。
- 响应/日志**逆序**（路由 → 全局）：先发布者最后撤销，与中间件语义一致。
- `on_response` 在 `upstream_response_filter` 里执行，早于下行写出，因此插件仍可改状态与头；
  `on_response_body` 在 `upstream_response_body_filter` 里逐块执行，此时头已经发出，所以它只能改**字节**，
  改头是 `PhaseViolation` 而不是无声失效（`response_filter` 跑在 `upstream_response_filter` 之后、字节之前，
  它只保留 1xx 保护与预算复核）。

### 3.1 Pingora 归属表（谁负责什么）

| Pingora 能力 | 归属 | 插件可见 |
|---|---|---|
| `early_request_filter` / `proxy_upstream_filter` | 数据面（我们不用；拒绝在更早的 `request_filter`） | ❌ |
| 逐跳头剥离、`Connection` 提名头剥离与保护字段拒绝 | **框架**（`HttpUpstreamRequestPolicy` 默认全开；`proxy_common.rs:178-189`） | ❌（插件写的逐跳头会被剥离） |
| 请求帧化 finalize（缺帧化时补 chunked） | **框架**（`proxy_common.rs:383-394`，在我们之后执行） | ❌ |
| 请求头/URI 改写 | 插件（`on_request`）→ 数据面 reconcile | ✅ |
| 连接事实（`connected_to_upstream`/`fail_to_connect`） | 数据面 | 间接（未来以 `ctx` 事实暴露，见 §11） |
| `fail_to_proxy` / `error_while_proxy` / `logging` | 数据面（唯一写、唯一记） | ❌ |

**帧化守卫**：框架只"补缺"，不纠正**错误的** `content-length`（`client.rs:775-799` 按头选写入器）。
所以 `request_filter` 在跑插件链**之前**快照 `content-length`/`transfer-encoding`，链后恢复——帧化是数据面的。

## 4. 请求体两种模式

**顺序事实**：`request_filter` → `upstream_peer` → `upstream_request_filter`（头已发出）→ `request_body_filter`（逐块）。
头先发，因此"整份改写"不能靠事后修补，只能**扣住 body 不发**。

### 4.1 默认：逐块（A）

- 每个块调用一次 `on_request_body`，`request_body()` 返回当前块，`set_request_body` 替换它；超预算 → 408。
- `body_complete()` 反映框架的结束标记：长度已知的最后一数据块为 `true`；chunked 流的终止段是空块、不作为块交付，
  所以它的数据块一律 `false`。要在 chunked 上传结束时做整体处理，只能声明 `buffers_body`。
- 开销：每块 × 每插件一次跨边界调用（WASM 更贵，文档明示"控制链长"）。

### 4.2 声明式整块（B，仅请求侧）

- 配置 `buffers_body: true`；编译期标记**该路由为整块模式**（全局链或路由链上任一插件声明即为整块）。
- 机制：
  1. `upstream_request_filter`：整块路由**移除 `content-length`、写入 `transfer-encoding: chunked`**
     （Pingora 写入器 TE 优先，`client.rs:776`），于是长度不确定也不会截断。
  2. `request_body_filter`：每块返回 `Some(Bytes::new())` 扣住（h1 写出器不会把 0 字节当终止块，
     `proxy_h1.rs:1057-1060`），同时把块累进缓冲；超过 `request.max_body_bytes` → 413（与 `request.max_body_bytes` 的拒绝路径同一处）。
  3. 流结束：调用一次 `on_request_body`（`body_complete() == true`），插件可整体改写；
     数据面把最终缓冲作为最后一块发出并 `end_of_stream = true`。
- 代价（必须写明）：上游在客户端传完之前收不到 body（延迟 = 上传时长）；内存 = 生效 body 上限 × 并发整块请求数。

### 4.3 响应体永远逐块

- `on_response_body` 每块一次，可替换/丢弃；**不允许**整块（SSE 可能无限长）。该阶段是同步的，插件不得阻塞，
  也不得在这里改响应头（头已发出）。chunked 响应的终止段同样不作为块交付，所以 `body_complete()` 不能用来
  判断事件流结束。
- 帧化：全局链或路由链声明 `RESPONSE_BODY` 时，`upstream_response_filter` 移除上游 `content-length`，让 Pingora 走 chunked
  （`proxy_h1.rs:938-953` 在既无 TE 又无 CL 时补 chunked），否则插件改变块大小会与 CL 不一致。

## 5. 文档模型：定义表 + 两个使用点

### 5.1 条目是实例，不是 kind 的别名

`plugins:` 只**定义实例**：`name`（唯一）+ `kind`（不透明字符串）+ 信封字段（`enabled` / `buffers_body` /
`error_response`）+ `config`（由 kind 的工厂在发布期用 `deny_unknown_fields` 解释）。它不决定在哪里运行；
两个**使用点**是 `global_plugins: [name]`（所有请求，路由匹配之前）与 `routes[].plugins: [name]`（该路由）。
三处引用都必须存在，同一列表内不得重复。

四条规则，都是可验证的：

| 规则 | 含义 |
|---|---|
| **一个条目 = 一份配置 + 一份运行期状态** | 密钥在发布期 materialize、拒绝模板在发布期编译、限流桶表挂在实例上 |
| **参数不同 = 另一个条目** | 同一 kind 的两条路由要不同 `burst` / 不同 `keys`，就声明两个实例（§5.3） |
| **共享实例 ≠ 共享配额** | 同一实例被 N 条路由引用：认证与密钥共享，限流桶仍按 `(route_id, 实例名, subject\|ip)` 分开 |
| **声明 ≠ 执行** | 空 `global_plugins` 是默认值；只有被使用点列出的实例才会运行 |

一个实例可以同时出现在两个作用域：编译一次，两条链共享同一个 `Arc<dyn Plugin>`。全局阶段没有 route id、
没有身份，所以全局限流只能按 `client_ip_hash` 键；按 subject 限流必须放在路由作用域。

### 5.2 完整示例

```yaml
apiVersion: gateway/v1

request:                                  # 每请求预算。路由只能收紧，不能放宽。
  max_body_bytes: 67108864                # 64 MiB 文档上限；整块模式内存 = 该值 × 并发整块请求
  timeout_ms: 120000                      # 2 分钟文档上限；1..=600_000

providers:
  - name: openai-primary
    kind: openai
    base_url: https://api.openai.com
    credential_ref: env:OPENAI_API_KEY

upstreams:
  - name: api-backend
    scheme: http
    host: 10.0.0.12
    port: 8080
  - name: upload-backend
    scheme: https
    host: uploads.internal
    port: 443

# 定义表：只声明实例。一个条目 = 一份 config + 一份运行期状态。
plugins:
  # 同一个 kind 的两份实例：不同的密钥集，因此是两个条目。
  - name: jwt-partner-a
    kind: jwt
    config:
      keys:
        - kid: a-2025-11
          secret_ref: env:PARTNER_A_JWT
      issuer: https://id.partner-a.example
      audience: nico-api

  - name: jwt-partner-b
    kind: jwt
    config:
      keys:                               # 多键集合：每一把都必须有 kid（轮换用）
        - kid: b-2025-11
          secret_ref: env:PARTNER_B_JWT
        - kid: b-2025-08
          secret_ref: env:PARTNER_B_JWT_PREV
      issuer: https://id.partner-b.example
      audience: nico-api
      leeway_seconds: 60                  # 默认 30
    error_response:                       # 每实例自定义拒绝呈现；状态只允许 4xx
      status: 401
      content_type: text/html
      body: "<h1>${code}</h1><p>${message}</p><p>trace ${trace_id}</p>"

  # 边缘 API key：全局与某条路由共用同一个实例。
  - name: edge-firewall
    kind: api_key
    config:
      header: x-edge-key
      keys:
        - { name: edge, secret_ref: env:EDGE_KEY }

  # 两个限流实例：同一 kind，不同数值。桶按 (route_id, 实例名, subject) 分开。
  - name: quota-free
    kind: rate_limit
    config: { burst: 100, per_second: 50, identity: subject, max_keys: 10000 }
  - name: quota-paid
    kind: rate_limit
    config: { burst: 20, per_second: 5, identity: subject }

  # 停用的实例：仍可被路由引用，但不编译、不执行（运营商紧急停用不必改路由）。
  - name: legacy-jwt
    kind: jwt
    enabled: false
    config:
      keys: [{ secret_ref: env:LEGACY_JWT }]

  # 需要整份请求体的实例，并可整体改写；声明后本路由进入整块模式。
  - name: upload-guard
    kind: api_key
    buffers_body: true
    config:
      header: x-upload-key
      keys: [{ name: uploader, secret_ref: env:UPLOAD_KEY }]

# 全局使用点：这些名字在所有请求上、路由匹配之前运行。
# 列表顺序只决定同阶段内的先后；认证类永远先于限流类。
global_plugins: [edge-firewall]

routes:
  - id: partner-a-api
    host: api.partner-a.example
    path_prefix: /v1/
    upstream: api-backend
    max_body_bytes: 1048576               # 只能收紧（≤ 文档值）
    timeout_ms: 5000
    plugins: [jwt-partner-a, quota-free]  # 写反也一样：认证先于限流

  - id: partner-b-api
    host: api.partner-b.example
    path_prefix: /v1/
    upstream: api-backend
    plugins: [jwt-partner-b, quota-paid]

  # 这条路由没有插件：只有全局链在它上面运行。
  - id: health
    path_prefix: /healthz
    upstream: api-backend
    timeout_ms: 1000
    max_body_bytes: 4096

  - id: uploads
    path_prefix: /upload/
    upstream: upload-backend
    max_body_bytes: 67108864              # 等于文档值（合法），但仍是"只能收紧"
    plugins: [upload-guard]
```

`nico-core` 拥有并校验：`name`（唯一）、`kind`（非空字符串）、`enabled`、`buffers_body`、`error_response` 的形状、
`global_plugins` 与 `routes[].plugins` 的引用存在性与列表内唯一。它**不拥有**任何 kind 的字段：`jwt` 的 `keys`、
`api_key` 的 `header`、`rate_limit` 的 `burst` 都在 `config` 里，由各自的工厂在发布期用强类型 +
`deny_unknown_fields` 反序列化。

### 5.3 同一 kind、不同参数

三种写法，按优先级选：

| 需求 | 写法 | 结果 |
|---|---|---|
| 两条路由用**不同**参数 | 两个实例，各自被引用（上面的 `jwt-partner-a` / `jwt-partner-b`、`quota-free` / `quota-paid`） | 两份编译产物、两份状态；`authn` 日志分别报两个实例名 |
| 多条路由用**相同**参数 | 一个实例，被多条路由引用 | 一份编译产物；限流桶仍按路由分开 |
| 一条路由挂**两个同 kind** 实例 | `plugins: [jwt-a, jwt-b]` | 两个都跑（认证都过才算过）；这是"按名字引用"相对 Envoy/Kong 默认模型的优势 |

**不能用 YAML anchor/alias 复用参数**：文档解析阶段会拒绝 `&name` / `*name`（防锚点扩张攻击，
`config/parse.rs::reject_aliases`）。参数重复的缓解手段是生成器（Helm / Kustomize / 控制面模板），
不是 YAML 技巧。

### 5.4 本期不支持：路由级参数覆盖与按使用点关闭

- `routes[].plugins` 的元素是**名字字符串**，不能写成 `{name, burst: 20}`（schema 拒绝）。路由级覆盖会让
  同一个名字对应多份有效配置，破坏"编译一次"与"名字即身份"。
- `enabled: false` 是**实例级**：无法表达"全局默认开、只对某条路由关"。需要时的最小形态是给使用点加
  `{name, disabled: true}`（仍是信封字段、仍归 core 校验），不是 per-route config。
- 若将来确实需要"共享重状态 + 路由级小旋钮"，升级形态是 `{name, use: {burst: 20}}`：`use` 的结构由 kind 的
  工厂解释、只允许收紧、编译期把 `(name, use)` 折叠成配置指纹（APISIX 把 `crc32(conf)` 放进桶键，配置一变
  计数器重开；我们的 `reuse_or_new` 用"实例名 + 参数相等"已经等价）。
- **触发条件**（满足任意两条才动）：① 同 kind 实例 ≥5 且只差 1–2 个标量；② 出现共享重状态的硬需求
  （例如 RS256/JWKS 的同一份 JWKS 缓存）；③ 人手维护 YAML 且已发生"改 A 忘 B"。业界对照与出处见
  `research/plugin-configuration-models.md`。

## 6. 编译：文档 → 运行时计划

`PluginRuntime::compile(&Document) -> Result<PublishedChains, PluginError>`（在 reload 的 validate-then-swap 里）：

1. 每个条目：`enabled: false` → 记一行 `disabled` 日志、跳过；否则按 `kind` 查注册表，缺失即
   `PluginError::UnknownKind`；工厂 `build(&spec)` 里反序列化 `config`、materialize 密钥、编译呈现模板，
   产出 `Arc<dyn Plugin>`。
2. 作用域与阶段排序：全局链 = `global_plugins` 列表（认证 → 限流，列表序）与每路由链（同规则）。
   一个实例可以同时出现在两个作用域里：编译一次，两个链共享同一个 `Arc<dyn Plugin>`。
3. **整块与响应重写判定**：同时检查全局链与每条路由链。全局条目声明 `buffers_body` 时所有路由进入整块模式，
   路由链上的同类声明只影响该路由；全局条目声明 `RESPONSE_BODY` 时所有路由都视为响应重写，路由链上的同类声明
   只影响该路由。结果分别记入 `PublishedChains::{buffered, rewriting_responses}`（hooks 读发布值，不在请求路径
   判断配置）。
4. **覆盖检查**：任何 `route.plugins` 非空的路由必须有链，否则编译失败（fail-closed）。
5. intern：引用列表相同的路由共享同一条链。
6. `PublishedChains` 随快照一起发布：`HttpState { snapshot, policies: PublishedChains }`，其中
   `PublishedChains { global: PluginChain, routes: RouteChains, buffered: BufferedRoutes, rewriting_responses: ResponseRewrites }`。

跨 reload 状态由**工厂**持有（§2.2）：令牌桶表按 `name + 参数指纹` 复用，用 `Weak` 记录，表随最后一代引用
消失；`PluginRuntime` 只是"注册表 + 编译"，不带运行期状态。整块模式无跨请求状态。

## 7. 拒绝与响应的四条呈现路径

| 来源 | 形态 | 谁写 |
|---|---|---|
| 数据面失败（404/413/502/503/504…） | `HttpError::*` 的默认映射 | `write_respond` |
| 插件拒绝，默认呈现 | `PluginError::Refused(默认 Rejection)` | `write_respond` |
| 插件拒绝，实例自定义呈现 | `PluginError::Refused(error_response 编译产物)` | `write_respond` |
| 插件合成响应 | `Outcome::Respond(ResponseData)` | `write_synthetic`（清洗后，与 `write_respond` 同在一个模块） |

清洗规则（合成响应用）：去逐跳头（含 `connection` 提名的头）；忽略插件给的 `content-length`（按实际 body 重算）；
`www-authenticate`/`retry-after` 仍只由 `Rejection` 路径产生；其余合法 end-to-end 头按原序保留（mock 是一个真答案，
`location`/`set-cookie` 必须能过）；1xx/204/304 不带 body。

**正约束：`Respond` 只从请求阶段携带。** 合成响应意味着数据面要用它替换正在进行的转发；在响应阶段替换需要
拦截上游 body，不在本期范围。`on_request_body`/`on_response`/`on_response_body` 返回 `Respond` 时按
`HttpError::Internal` 处理（消息写明"a synthesised response is only carried from the request stage"），已提交后
则 abort；`on_logging` 返回 `()`，不携带控制流。

## 8. 模块与文件预算

单个 `.rs` 文件的硬上限是 500 行，由 pre-commit 门禁检查。行数是从代码派生的，不在设计里复述；新代码接近上限时按
`foo.rs` + `foo/` 拆到业务边界，父文件与子模块各守一个边界。

| 范围 | 文件 | 拥有 |
|---|---|---|
| `crates/nico-http/src/` | `plugin.rs` | `Plugin`/`Outcome`/`PluginInfo`/`PluginContext` 契约 |
| | `plugin/chain.rs` + `plugin/chain/tests/` | 五阶段驱动，及其按控制流 / 字节 / 共享替身分开的测试 |
| | `plugin/context.rs` + `plugin/context/` | 数据面侧 `PluginContext` 实现；`contract.rs` 承载 trait 实现，`phase.rs`/`caller.rs` 各拥有一个判定 |
| | `context.rs` + `context/` | 请求级事实；`identity.rs` 与 `buffer.rs` 各拥有一个数据结构 |
| | `plugin/response.rs` | `ResponseData` 构造校验与 `clean_response` |
| | `plugin/header.rs` / `plugin/error.rs` / `plugin/phases.rs` / `plugin/registry.rs` | 头文法、错误类型与唯一转换、阶段位集合、工厂注册表 |
| | `state.rs` | `HttpState`/`HttpStateHandle`/`PublishedChains`/`RouteChains`/`BufferedRoutes`/`ResponseRewrites` |
| | `proxy/hooks.rs` | 唯一 `impl ProxyHttp` 与逐方法委派 |
| | `proxy/hooks/{request,body,response}.rs` | 请求、请求体、响应三个方向的钩子体 |
| `crates/nico-plugins/src/` | `compile.rs` | 文档 → `PublishedChains`：注册表查找、排序、整块与响应重写判定、intern、覆盖检查 |
| | `<kind>.rs` + `<kind>/{config,factory,json,tests}.rs` | 每个 kind 的 `Plugin` 实现、配置、工厂、JSON 辅助与测试；这是 `foo.rs` + `foo/` 的既定拆分 |
| `crates/nico-core/src/` | `config/request.rs` | `RequestBounds { max_body_bytes, timeout_ms }` 与 `Document::request()` |
| | `config/plugin.rs` | 根 `plugins` 的信封及使用点引用校验 |

## 10. 观测

- 请求日志一条（数据面），`authn = 认证插件实例名`；插件 `log()` 按插件名归属。
- 编译期：每个插件一行 `info`（scope/kind/阶段/是否整块），密钥只报引用名。
- 不记 body、凭证、明文 IP。

## 11. 风险与已知缺口

| 风险 / 缺口 | 处置 |
|---|---|
| 逐块跨边界调用的成本（WASM 更明显） | 文档明示；链长是配置决定；WASM 里程碑可加"每插件预算" |
| 整块模式的内存 = 文档里的 `request.max_body_bytes` × 并发整块请求 | 该值不设范围（`u32` 即边界）：配置作者写多大就允许占多大。风险表写明这个算术，不用代码去猜一个上限 |
| 整块模式的上游延迟 = 上传时长 | 写明；只在插件声明时启用 |
| 全局插件身份为空 | 全局限流只能按 IP 哈希键；按 subject 限流放路由作用域（文档化） |
| 连接事实（复用/连接失败）插件看不到 | 记为缺口；未来以 `ctx` 事实（`connected/reused/failed`）暴露，非钩子 |
| 插件出站 HTTP（WASM `host_http_request`） | WASM 里程碑；届时需 SSRF/超时/响应大小策略 |
| kind 配置由工厂校验，core 只拥有信封 | 校验仍发生在发布期（`compile`），失败仍保 LKG；错误里带插件名与字段路径；core 不解释 kind 字段，使新增 kind 不改文档模型 |
| WASM guest 必须是同步投影 | 写进 §2.2 契约；若将来要 guest 内部异步，那是 WASM 里程碑的独立设计题，不改本任务的 trait |
| 响应体阶段是同步的 | `on_response_body` 是唯一的同步阶段（Pingora 的 `upstream_response_body_filter` 是同步钩子），插件不得在此阻塞；预算由钩子在阶段前后复核，而不是包裹该同步调用。WASM 的同步投影与该约束一致 |
| `Respond` 只从请求阶段携带 | 响应阶段替换需要拦截上游 body，本期不做（§7） |
| 注册表缺工厂 = 静默放过的诱惑 | 未知 kind 直接拒绝文档（fail-closed）；`enabled: false` 才是有意的跳过 |
| 参数复制：N 条路由要不同 `burst`/`keys` = N 个实例 = N 份 `config` | 文档写明"参数不同即另一个条目"；YAML anchor/alias 被解析阶段拒绝（`parse.rs::reject_aliases`，防锚点扩张），所以不能靠 anchor 复用，缓解手段是生成器（Helm/Kustomize）。升级到 provider+selector 的触发条件见 §5.4 |
| 缺少"按使用点关闭" | `enabled: false` 只停实例；"全局默认开、个别路由关"需要给使用点加 `{name, disabled: true}`（仍是信封字段）。触发条件出现前不做，记在 §5.4 |
| 模型作用域插件 | `1.4 nico-ai` |

## 12. 交付边界

- **`nico-core`**：`request:` 块由 `RequestBounds`（字段 `max_body_bytes`/`timeout_ms`，按 nginx 的「主语 + 资源」
  风格命名）表示，`Document::request()` 暴露它；`max_body_bytes` 只受 `u32` 表示范围约束，`timeout_ms` 受
  `1..=600_000` 约束。根 `plugins` 是**信封**（`name`/`kind`/`enabled`/`buffers_body`/`error_response`/`config`），
  `config/plugin.rs` 只拥有信封与引用校验；三个 kind 的配置类型由 `nico-plugins` 拥有。
- **`nico-http`**：`plugin.rs` 拥有 `Plugin`/`PluginFactory`/`PluginRegistry`/`PluginSpec`/`Outcome`/`ResponseData`/
  `PluginInfo`/`PluginError`/`PluginContext`，`plugin/{context,response,chain}.rs`、`state.rs`、
  `proxy/{hooks,record}.rs` 分别拥有上下文、清洗、链驱动、发布值与代理记录。请求路径由 `Plugin` 五阶段驱动，
  不导出 Pingora 头类型；请求方向的逐跳头与 `Connection` 提名头由框架 `HttpUpstreamRequestPolicy` 处理。合成响应
  清洗保留一份逐跳表（`plugin/header.rs`），因为响应不经过框架的请求钩子。
- **`nico-plugins`**：`<kind>.rs` 与 `<kind>/` 子模块构成 `foo.rs` + `foo/` 拆分，配置结构体、工厂、`Plugin`
  实现与测试按 kind 归位；`compile.rs` 按注册表产出 `PublishedChains`。
- **`nico-server`**：注册内置工厂（`PluginRegistry`）、发布 `PublishedChains`；`config/resources.yaml` 示例含全局插件、
  `enabled` 与 `buffers_body`。
- **spec**：`pingora-guidelines.md`（归属表、hold-back、帧化守卫）、`error-handling.md`（四条呈现路径）、
  `logging-guidelines.md`（`authn`、插件日志归属）、`directory-structure.md`（模块树与可见性）。
