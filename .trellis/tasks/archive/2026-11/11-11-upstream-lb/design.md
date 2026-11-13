# 设计：流量层负载均衡（同一 upstream 的多地址）

## 1. 不变量与失败模式

| 不变量 | 违反时的失败模式 |
|---|---|
| **I1 只有一个选择点。** 从"成员的候选集"到"这次连接哪个地址"只发生一次，就在组装 `Destination` 的地方。 | 两个地方各挑一次，一次请求的首跳与重试可能落到不同实例而没有记录能解释。 |
| **I2 SNI 与 `Host` 永远来自逻辑身份。** 连接目标是 `address`，证书校验名与 `Host` 是 upstream 的 `host`。 | 同一 vhost 多机部署下证书校验失败；或更糟——把 `Host` 写成内网 IP，上游按错误的 vhost 路由。 |
| **I3 运行态只在身份相同处继承。** reload 换了地址或端口，旧状态必须丢弃，不能"继承"到一台没见过的新机器。 | 新实例一上线就带着旧实例的连续失败计数，或旧的 open 状态把新实例挡在门外。 |
| **I4 全部被剔时显式失败。** 返回 `502 upstream_unreachable`，不把请求送给一个刚被判定失败的地址。 | 延迟叠在失败上，而调用方看到的是一个"网关超时"而不是"后端不可用"。 |
| **I5 选择状态不进入快照。** 计数器、活跃连接数、剔除表都是进程级运行态，发布只换文档。 | 每次 reload 把计数清零，`least_conn` 与熔丝在发布后瞬间失真。 |

## 2. 选择发生在哪

```
route ──► upstream（逻辑身份：scheme, host, port）
              │
              ├─ 候选集：endpoints[]（每项 name/address/port/weight）
              ▼
        UpstreamPools（进程级运行态：轮转位置、活跃连接、剔除表）
              │  选一个成员
              ▼
   Destination { scheme, host = address, port = endpoint.port, authority = upstream.host }
              │
              ├─ upstream::peer()  → 解析 address、建 HttpPeer(tls, sni = authority)
              └─ upstream::host_header() → authority（不是 address）
```

插入点只有一处：`proxy/forward.rs::destination` 里 `Destination::from(upstream)` 那一行。普通转发与
LLM 路由的 provider 目标都不经过它（provider 的 `Destination` 由 `nico-llm` 给出），所以 LLM 路径
行为零变化。

## 3. `Destination` 增加 `authority`

今天 `Destination { scheme, host, port }` 同时承担"连哪里"与"证书/`Host` 叫什么"。多地址之后这两件事
必须分开：

```rust
pub struct Destination {
    scheme: Scheme,
    host: Arc<str>,      // 连接目标：endpoint 的 address（单实例形态下就是 upstream.host）
    port: u16,
    authority: Arc<str>, // Host 头与 SNI：upstream.host
}
```

- 现有构造函数保持行为不变：`Destination::new(scheme, host, port)` 让 `authority == host`，因此
  `nico-llm` 与既有测试零改动。
- 新增 `Destination::with_authority(self, authority)`，只由 upstream 的选择路径使用。
- `host_header()` 与 `peer()` 里的 SNI 改读 `authority`；`resolve()` 继续读 `host`。这三处是唯一读
  这两个字段的地方（现在 `host_header` 用 `host` + `port` 判断默认端口，改读 `authority` + `port`）。

## 4. 配置模型

`nico-core` 的 `Upstream` 从"一个地址"变成"逻辑身份 + 一组地址"：

```rust
pub struct Upstream {
    name: Arc<str>,
    scheme: Scheme,          // 一个 upstream 只有一种传输方式
    host: Arc<str>,          // 逻辑身份：Host + SNI
    port: u16,               // 默认端口
    endpoints: Arc<[Endpoint]>, // 至少一个成员
}
pub struct Endpoint { name: Arc<str>, address: Arc<str>, port: u16, weight: u16 }
```

- 单实例写法（`scheme` + `host` + `port`，无 `endpoints`）在解析期展开成
  `[Endpoint { name: "default", address: host, port, weight: 1 }]`。**文档层没有"单实例"这个分支**，
  数据面因此也只有一种形状。
- endpoint 不携带 `scheme`：混协议的一个 upstream 是两台不同的服务，应该写成两条。
- 校验（发布期，`deny_unknown_fields` 已把未知字段挡在外面）：
  - `endpoints` 非空；`name` 在 upstream 内唯一；`address` 通过 `validate_host`（或 IP 字面量）；
  - `weight` `1..=1000`（流量层没有"只兜底"的语义，所以不接受 0）；
  - 成员数上限 `schema_bounds.max_endpoints`（默认 64，新增的第五个有界字段）。
- `Destination::from(&Upstream)` 不再直接可用（它没有成员可挑），改为
  `Upstream::endpoints()` 提供候选，由选择器产出 `Destination`。

## 5. 运行态：`UpstreamPools`

住在 `HttpApp`（进程级），**不在** `HttpState` 里，因此 reload 不重建它：

```rust
pub(crate) struct UpstreamPools { /* 每 upstream 一份 */ }
struct UpstreamState {
    picker: Arc<dyn Picker>,           // 策略及其状态都封在这后面（见 6.1）
    ejection: Ejection,                // 判成员用的失败预算
    members: Vec<Member>,              // 与 endpoints 一一对应，每项带本代 weight
}
struct MemberState {                   // 跨发布继承，按成员身份寻址
    active_connections: AtomicU32,     // least_conn
    failures: AtomicU32,               // 连续失败数
    opened_at: Mutex<Option<Instant>>, // 剔除开始的时刻
    probing: AtomicBool,               // 半开：只允许一个试探
}
```

- **身份**：`(upstream 名, endpoint 名, address, port)`。reload 时只有这四元组完全相同的成员继承
  状态，其余归零（I3）。这与 provider 熔断器"配置未变的目标继承历史"同源。
- 发布时把新的成员表与旧的按身份对齐（一次性、在发布线程上做），得到新的 `Vec<Member>`；
  数据面只读。
- 策略自己的状态（轮转位置、平滑权重）不在这里：它跟着 `Picker` 走，并且只在文档仍然指定同一
  策略、且成员身份未变时由 `Picker::inherit` 迁移（见 6.1）。池因此不知道任何策略的状态布局。

## 6. 策略

| 策略 | 实现 | 状态归谁 |
|---|---|---|
| `round_robin` | 每候选一次，按位置轮转 | `RoundRobin` 自己的计数器 |
| `weighted_round_robin` | nginx 的平滑加权轮转：每候选 `current += weight`，选 `max(current)`，选中者 `current -= total` | `SmoothWeighted` 自己的 `current` 向量 |
| `least_conn` | 在候选里取 `active` 最小者，平票轮转 | `LeastConn` 自己的平票计数器；活跃数仍属成员 |
| `ip_hash` | 会合哈希（rendezvous）：对每个候选算 `hash(client_ip, endpoint_name)`，取最大者 | 无状态 |

- 只有健康成员进入候选；全部不健康 → `502 upstream_unreachable`（I4）。
- `least_conn` 的计数由 `HttpContext` 持有一个 guard：选中时 `+1`，请求结束（含失败、取消、
  短路响应）时由 `Drop` `-1`。只有 `Drop` 一条递减路径，避免"某个失败分支忘了减"。
- `ip_hash` 用会合哈希而不是取模：成员增删时取模会把几乎所有键重新映射，会合哈希只迁移
  `1/n`，对有状态的上游或缓存是决定性的差别。

### 6.1 抽象：`Picker` trait

四种策略曾写成四个签名互不相同的自由函数（`position+candidates` / `weights+current` /
`load+tie` / `key+names`），而状态留在 `UpstreamState` 的字段里。那不是抽象：池知道每个策略的
状态布局并替它预组装输入，加一个策略要同时改池、选择模块和一处 `match`。

现在的契约：

```rust
pub(crate) trait Members {
    fn len(&self) -> usize;
    fn weight(&self, index: usize) -> u16;
    fn active(&self, index: usize) -> u32;
    fn name(&self, index: usize) -> &str;
}

pub(crate) struct View<'a> {
    members: &'a dyn Members,
    candidates: &'a [usize],
    client: Option<IpAddr>,
}

pub(crate) trait Picker: Send + Sync + std::fmt::Debug {
    fn pick(&self, view: &View<'_>) -> Option<usize>;
    fn inherit(&self, previous: &[Arc<str>], current: &[Arc<str>]) -> Arc<dyn Picker>;
}
```

五条约束，每条都对应一种会静默出错的写法：

1. **状态归策略实现所有。** `UpstreamState` 只持有 `Arc<dyn Picker>`，不知道里面是计数器、
   向量还是什么都没有。原来每个 upstream 都背着一个用不上的 `smooth` 锁，现在只有
   `WeightedRoundRobin` 付这份代价，而它恰好也是唯一读它的策略。
2. **输入是访问器，不是预组装数组。** 策略只读它需要的字段：`ip_hash` 不问 weight，
   `round_robin` 不问 name。加一种输入是加一个访问器，而不是给四个实现各加一个参数。
3. **`Members` 是第二个池的接缝。** provider 池实现同样四个问题就能复用同一套策略——这是
   `11-11-model-lb` 能复用流量层选择算法的前提。
4. **`inherit` 按身份迁移，不按下标。** 平滑权重属于某台机器，成员换地址就是一个新成员；
   轮转位置属于列表，跨发布保留。策略自己决定它的状态是哪一类，池不再知道。
5. **`build` 是唯一且穷尽的 `Strategy → impl` match。** `Strategy` 因此去掉
   `#[non_exhaustive]`：新增变体必须在这里编译失败，直到为它实现一个 `Picker`。此前那处
   `_ => round_robin` 通配会让新策略**编译通过并静默按轮转运行**。

代价（显式接受）：每次选择一次 vtable 调用（相对一个 socket 可忽略）；发布期多一步构造与
`inherit`。收益：`picker` 模块成为可以整体搬走的单元（trait + 四个实现，不碰 `Destination`
与 `HttpContext`），加一个策略 = 一个 `impl Picker` + `build` 的一个分支。

## 7. 失败计入与剔除

| 事件 | 计入该成员？ | 说明 |
|---|---|---|
| 连接失败 / 解析失败 / 连接超时 | 是 | 现有的 `fail_to_connect` 与 `upstream_peer` 的 `Err` 路径 |
| 上游 5xx | 是 | 它答了，但答的是"我这里坏了" |
| 上游 4xx | 否 | 调用方的问题；计入会让一个坏客户端踢空整组 |
| 客户端取消 / 网关停机 | 否 | 与被选中的成员无关 |

- 连续失败达 `max_fails`（默认 3）→ 剔除，记录 `opened_at`。
- 冷却 `fail_timeout_ms`（默认 30000）后进入半开：放行**一个**试探（`probing` 从 false 交换到 true
  成功者占位，其余继续视为不健康）。试探成功 → 清零并回归；失败 → 重新计时。
- 成功响应（任意 2xx/3xx/4xx）清零连续失败数：4xx 不计入失败，但它也证明这条链路是通的。
- 剔除表与 LLM 的 provider 熔断器是两张表：前者按成员（地址），后者按 provider 目标。两者都不共享
  代码，因为候选身份与冷却语义不同；合并会让 "一台机器挂了" 与 "一家 provider 限流" 用同一个数字。

## 8. 可观测性

- 指标：`gateway_upstream_selected_total{upstream, endpoint}`、
  `gateway_upstream_ejected_total{upstream, endpoint}`（剔除事件，不是状态）。
- 日志：请求记录保持 `upstream` = upstream 名；有多成员时新增 `upstream.endpoint`。剔除/回归各一条
  `warn!` / `info!`，字段 `upstream`、`upstream.endpoint`、`reason`（`connect` / `timeout` / `5xx`）。
- 单成员部署的日志与指标形状不变（不写 `upstream.endpoint`）。

## 9. 备选方案与取舍

| 备选 | 为什么不做 |
|---|---|
| **启用 pingora 的 `lb` feature** | `LoadBalancer` 自带 `Backend` 与健康检查模型，选择状态封在它内部，既无法在 reload 时按身份继承，也不接受"SNI 来自逻辑身份而连接目标来自成员"这个分离。多一个依赖，换不到可用的行为。 |
| **`ip_hash` 用取模** | 成员增删会把几乎所有键重映射。会合哈希多 10 行，换到稳定性。 |
| **`least_conn` 读 Pingora 的连接池** | 池是进程级的、连接不属于某个请求，拿不到"此刻有多少请求正在用这个成员"。必须自己计数。 |
| **把剔除状态放进发布快照** | reload 清零（违反 I5）。 |
| **`weight: 0` 表示禁用** | 流量层没有兜底语义，0 权重的成员永远不会被选中，与 `enabled: false` 是同一件事但更隐晦；宁可只接受 `1..=1000`。 |
| **在 `Destination` 上重命名 `host`** | `nico-llm` 与大量测试都直接构造 `Destination`；加一个带默认值的 `authority` 是零破坏的。 |

## 10. 兼容性与回滚

- 文档兼容：不写 `endpoints` 的文档解析结果与今天等价（一个成员、权重 1），所有既有 e2e 与 fixture
  零改动。
- 行为兼容：单成员时 `round_robin` 恒选它；剔除后立即 502 —— 与今天"连不上就 502"的区别只是错误
  在到达连接函数之前产生，对外 code 相同。
- 回滚形状：改动集中在 `nico-core` 的 `Upstream`/`RawUpstream`、`nico-http` 的 `upstream/{picker,pools}`、
  `forward::destination`、`Destination` 的一个字段与两处读取、发布链路的成员对齐、以及文档/指标。
  回滚 = 删掉端点列表（保留单地址）与选择器调用，无数据迁移。

## 11. 测试策略

| 层 | 内容 |
|---|---|
| 纯单测 | 四种策略的选择序列（注入确定性时钟/计数器）；平滑加权轮转的比例与顺序；会合哈希在成员增删时的迁移量；权重边界被拒 |
| 状态机单测 | 连续失败达阈值剔除、半开只放一个试探、试探成功/失败、4xx 不计入、成功清零 |
| 继承单测 | 发布前后同身份成员保留计数、改地址归零、删成员丢弃、加成员初始为零 |
| 集成 e2e | 一个 upstream 两个实例：杀掉一个 → 后续请求全部落到另一个（断言真实收到的请求，不用 sleep 猜）；恢复后冷却结束才回归 |
| 接缝 e2e | 多实例 + https：证书签的是逻辑 host，`Host` 与 SNI 都是逻辑 host，握手成功 |
| 回归 | 单实例文档的既有 e2e 全部零改动通过 |
