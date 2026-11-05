# Rust 原生微内核 + 插件技术架构

> 状态：架构设计稿。阶段 0（Workspace 骨架与 local 配置源）已交付；HTTP 数据面尚未实现。
>
> 当前范围：API 网关 + AI 网关；Pingora 用于 HTTP 数据面；WebRTC 暂缓。
>
> 配置源的一手资料核查见 `research/config-source-options.md`（etcd release-3.5 源码、Watch RPC 字段、etcd-client 0.21.x）。

## 1. Rust 设计原则

这里的“微内核”不是运行时插件容器，而是少量不依赖具体业务的公共模块、清晰的依赖方向和预编译配置快照。

1. **Rust-native composition**：Cargo 静态组合、模块优先、领域 trait、少量 trait object、显式构造；不构建通用插件容器。
2. **Module before crate**：先在一个 crate 内按模块划分，只有独立依赖、feature、复用或发布需求出现后才拆 crate。
3. **Static before dynamic**：第一版所有实现编译期确定，启动时显式构造；不开发动态插件加载器。
4. **Trait 放在领域拥有者中**：不建立包罗万象的 SPI crate。
5. **Enum/generics 与 dyn 各取所需**：闭合集合可用 enum，开放扩展点可用少量 `Arc<dyn Trait>`；以 profile 而非意识形态决定。
6. **配置类型化**：每个插件定义自己的 `serde::Deserialize` Config，不在热路径传递 `serde_json::Value`。
7. **网络热路径不做通用序列化**：原生模块之间传递 `Bytes`、HTTP 类型或具体领域结构。
8. **异步语义优先**：取消、timeout、背压、half-close 和 EndOfStream 是接口的一部分，而不是后续补丁。
9. **避免框架命名泛滥**：只有确实管理多种生命周期或算法时才使用 Manager/Factory/Service 等名称。

## 2. 推荐 Cargo Workspace

以下是**建议结构，不是已创建文件**：

```text
nico/
├── Cargo.toml                  # workspace 依赖与成员
├── Cargo.lock
├── rust-toolchain.toml
├── config/
│   ├── bootstrap.yaml        # 仅本地：配置源连接、缓存路径、资源上限
│   └── resources.yaml        # local source 时的网关资源
└── crates/
    ├── nico-server/         # 唯一 binary crate（二进制 `nico`）：启动、组装、reload、角色选择
    │   └── src/
    │       ├── main.rs
    │       ├── bootstrap.rs
    │       └── reload.rs       # 订阅源、去抖、single-flight、编译与发布
    │
    ├── nico-core/           # 真正跨数据面复用的小核心
    │   └── src/
    │       ├── lib.rs
    │       ├── config.rs       # 通用配置元数据、generation、secret ref
    │       ├── context.rs      # trace/peer/tenant/deadline/cancellation
    │       └── error.rs
    │
    ├── nico-http/           # Pingora 与 HTTP 领域
    │   └── src/
    │       ├── lib.rs
    │       ├── proxy.rs        # Pingora ProxyHttp 接入
    │       ├── route.rs        # HTTP RouteTable / CompiledRoute
    │       ├── filter.rs       # HTTP 专属 trait
    │       ├── upstream.rs     # peer 选择、连接与重试约束
    │       └── stream.rs       # HTTP body/stream 辅助
    │
    ├── nico-ai/             # AI 领域插件与 Provider 模块
    │   └── src/
    │       ├── lib.rs
    │       ├── filter.rs       # AI 请求元数据、模型路由
    │       ├── model.rs
    │       ├── provider/
    │       │   ├── mod.rs      # Provider trait
    │       │   ├── openai.rs
    │       │   └── anthropic.rs
    │       └── stream/
    │           ├── mod.rs
    │           ├── sse.rs
    │           └── anthropic.rs
    │
    ├── nico-config/         # ConfigSource trait、local 实现、测试 Fake
    │   └── src/
    │       ├── lib.rs
    │       ├── source.rs       # trait 与错误类型
    │       ├── revision.rs     # source-scoped revision/digest
    │       ├── local.rs        # 文件 + 可选 notify/SIGHUP
    │       ├── cache.rs        # last-known-good 本地缓存
    │       └── fake.rs         # 测试与故障注入
    │
    ├── nico-config-etcd/    # 可选：etcd v3 实现（重依赖，feature 开启）
    │   └── src/lib.rs
    │
    ├── nico-config-nacos/   # 可选：Nacos 实现（重依赖，feature 开启）
    │   └── src/lib.rs
    │
    └── nico-plugins/        # 第一期内置 HTTP 策略，避免大量微 crate
        └── src/
            ├── lib.rs
            ├── jwt.rs
            ├── api_key.rs
            └── rate_limit.rs
```

### 不为每个 Provider 单独建 crate

`openai`、`anthropic` 初期只是 `nico-ai::provider` 下的模块。只有以下情况才拆成独立 crate：

- 依赖很重或需要独立 feature。
- 需要被其他 Workspace 复用。
- 需要独立发布或由独立团队维护。
- 编译隔离带来的收益已超过 crate 数量成本。

## 3. Crate 职责与依赖方向

```text
nico-server
   ├──> nico-core
   ├──> nico-http ─────────> nico-core + pingora
   ├──> nico-ai ───────────> nico-http + nico-core
   ├──> nico-config ───────> nico-core
   ├──> nico-config-etcd ──> nico-config        (feature: config-etcd)
   ├──> nico-config-nacos ─> nico-config        (feature: config-nacos)
   └──> nico-plugins ──────> nico-http + nico-core
```

### `nico-core`

只保留已被多个数据面真实复用的内容：

- 通用配置版本、generation、secret reference。
- trace/peer/tenant/deadline/cancellation 等公共元数据。
- 公共错误类别，避免映射为某种协议响应。
- 必要的无业务工具，不包含 HTTP route、AI model 或 Pingora 类型。

**依赖约束**：`nico-core` 不得依赖 `nico-http`、`nico-ai`、`nico-plugins` 或 Pingora。

### `nico-http`

- 拥有 Pingora `ProxyHttp` 适配与原生 HTTP Session 生命周期。
- 定义 HTTP 专属 `HttpFilter`、路由、CompiledRoute 和 upstream 接口。
- 处理 request/response headers、body、stream、commit、cancel 和 upstream 错误。
- 不 import `nico-ai`；AI 通过 HTTP 扩展点接入。

### `nico-ai`

- 定义 `Provider`、模型映射、AI 请求元数据和 usage 类型。
- 实现 AI HTTP Filter 与 Provider Stream Adapter。
- OpenAI/Anthropic 是模块，不进入 `nico-core`。
- 可以依赖 `nico-http`，因为当前 AI API 就是 HTTP 领域能力。

### `nico-plugins`

- 放置第一期受信任的 JWT、API Key、Rate Limit 等内置实现。
- 当前实现 HTTP 策略时依赖 `nico-http` 是合理的，不为了“看起来通用”抽象成无类型 Metadata 框架。
- 当 MQTT 等第二个数据面确实能复用同一策略时，再把最小的通用部分下沉到 `nico-core`。

### `nico-config`

- 定义 `ConfigSource` trait、`Revision`、`ConfigVersion` 和配置源错误。
- 提供 `local` 文件实现、last-known-good 缓存和测试用 `FakeConfigSource`。
- 只依赖 `nico-core`、`serde`、`bytes`、`tokio-util`、`async-trait` 等轻量库。
- **不依赖** Pingora、HTTP、AI、etcd 或 Nacos 客户端。
- 配置源只加载和订阅字节文档，不解析路由、不修改运行时快照。

### `nico-config-etcd` / `nico-config-nacos`

- 分别实现 `ConfigSource`，隔离各配置中心的重量级依赖和版本特性。
- 仅通过 Cargo feature 编入 binary，未启用时不增加依赖和攻击面。
- 负责协议连接、认证、watch 重连和 revision 映射，不包含网关路由编译逻辑。

### `nico-server`

- 唯一的具体组装入口：创建 Provider、Filter builder、HTTP Proxy、SharedServices。
- 读取**本地 bootstrap 配置**选择唯一配置源，再启动订阅和发布管线。
- 执行配置加载、去抖、校验、编译、ArcSwap 发布和滚动退出。
- 不包含 Provider 算法、Filter 业务或配置中心协议细节。

## 4. Rust 插件就是“领域 trait + 静态实现”

### 4.1 Trait 归属

| Trait/抽象 | 所在 crate | 用途 |
|---|---|---|
| `HttpFilter` | `nico-http` | HTTP 请求/响应阶段扩展 |
| `Upstream` | `nico-http` | 上游选择与调用约束 |
| `Provider` | `nico-ai` | AI 请求/响应语义适配 |
| `ProviderStream` | `nico-ai` | 每个响应流独立的转换状态 |
| `ConfigSource` | `nico-config` | local / etcd 的加载与订阅；Nacos 保留后续实现位 |

不要在 `nico-core` 中提前定义 `ProtocolPlugin`、`PolicyPlugin`、`ConnectorPlugin`、`ExporterPlugin` 等一组没有多个真实消费者的 trait。

### 4.2 启动时显式注册

- `nico-server` 知道所有具体实现，并在启动时构造它们。
- 配置加载阶段可以用 `match plugin_type` 或 `HashMap<&str, Builder>` 将 YAML 类型名映射到构建函数。
- 字符串映射只允许出现在**配置编译阶段**；请求阶段使用已构建的 `Arc<dyn HttpFilter>`、`Arc<dyn Provider>` 或 enum。
- 新增 Provider 时修改 `nico-ai` 和 server 注册点即可，Kernel/HTTP 路由算法不变。静态组合不要求“新增实现完全零注册代码”。

### 4.3 Trait object 用在哪里

适合使用 `Arc<dyn HttpFilter>`：

- 路由需要按配置组合不同 Filter。
- Filter 在请求级或响应头级被调用，动态分发成本相对网络 I/O 很小。
- 多个无状态 Filter 可在线程间共享。

不一定适合：

- 每个 SSE 字节或 Chunk 都经过通用 Registry/Event Bus。
- 闭合且极热的 Provider 分支，可先用 enum 或具体类型。
- per-request/per-stream 状态被错误地塞进全局 `Arc<dyn Trait>` 实例。

`async fn` 需要 trait object 时，可按锁定 Rust 版本选择 `async_trait` 或 boxed future；不要假设原生 async trait 自动满足 dyn 要求。

## 5. 路由编译与请求上下文

### 5.1 CompiledRoute 而不是通用执行图框架

每条路由在 reload 时构建一个不可变 `CompiledRoute`，逻辑上包含：

- 已编译的 host/path/method 匹配信息。
- 按固定阶段排序的 HTTP Filter 引用。
- 上游或 AI Provider 引用。
- body/timeout/retry 等资源限制。
- 配置 generation。

HTTP 热路径只做：`RouteTable::match → &CompiledRoute → 依次调用 Filter`。不构建图对象、不查询 Registry、不读取控制面。

### 5.2 上下文按领域划分

- `nico-core::RequestMeta`：trace、peer、tenant、deadline、cancellation。
- `nico-http::HttpContext`：HTTP Session 所需状态、上游状态和 response commit。
- `nico-ai::AiRequestState`：逻辑/物理模型、Provider、streaming、usage。
- `nico-ai::ProviderStream`：每个上游流独立的增量解析状态。

AI 状态由 AI Filter 与请求本地状态关联，不放进 `nico-core`。如使用 `http::Extensions` 等类型化扩展存储，应限定在明确的请求边界，不能演变成字符串键值或无约束对象袋；普通请求不应创建 AI 状态。

## 6. 配置与热重载保持简单

### 6.1 配置编译

1. 从 `ConfigSource` 取得一份带 revision/digest 的完整文档。
2. 使用 `serde` 读取版本化 YAML。
3. 将每个插件配置反序列化为它自己的强类型 Config。
4. 校验路由、upstream、provider 引用和资源限制。
5. 构建新的 RouteTable、CompiledRoute 和必要的共享句柄。
6. 全部成功后形成一个不可变 RuntimeSnapshot。

不需要为每个插件实现一套通用 `prepare/commit/abort` trait。构建函数失败就丢弃候选快照；只有真正需要两阶段资源的组件自己处理回滚。

### 6.2 ArcSwap 发布

- `RuntimeSnapshot` 由 `ArcSwap` 持有。
- 请求开始时加载 `Arc<RuntimeSnapshot>`，整个请求/流固定使用该 generation。
- 新快照原子替换，旧快照随在途请求结束释放。
- reload 失败保留 last-known-good，并记录 generation、错误和 reload 时间。
- 不在 reload 时无条件重建所有连接池、限流器或 DNS 状态。

长寿命资源可由 `nico-server` 创建的 `SharedServices` 持有，快照中只保留 `Arc` 引用和对应配置版本。是否需要保留旧状态由具体资源决定，不使用通用 Lifecycle Manager 猜测。

### 6.3 两种升级

- **配置变化**：构建新快照，ArcSwap 热发布。
- **代码/feature/依赖变化**：重新编译 artifact，滚动重启并 drain。

### 6.4 与配置源管线的衔接

```text
ConfigSource::load / watch
        |
        v
去抖 + single-flight + revision 排序
        |
        v
解析 + schema/引用/资源校验（后台线程）
        |
        v
构建 RuntimeSnapshot
        |
    成功? --否--> 保留 LKG，标记 degraded，记录错误
        |是
        v
ArcSwap 原子发布 + 指标/审计
```

配置源永远不直接修改路由表；它只提供完整文档和变更事件。以下规则由 server 统一执行：

- 同一 digest 不重复编译。来源提供可比 revision 时丢弃回退/乱序事件；只有可判等 token 时，adapter 负责单连接内的顺序，server 不伪造全序。
- 去抖窗口与最大等待时间同时存在，避免频繁推送导致 reload 风暴，也避免持续变更造成饥饿。
- 同一时刻最多一个编译任务；新事件到达时只保留最新候选。
- 解析/编译不得阻塞 Pingora worker，放在后台任务中完成。
- 启动阶段没有可用快照时默认不 ready；只有显式启用 bootstrap cache 模式才允许用本地缓存启动并标记 stale。
- 集群内各实例只会最终一致地看到同一 revision，不承诺跨实例原子切换；路由变更必须向后兼容，先加后删。

## 7. 配置来源：local / etcd / Nacos

### 7.1 为什么这里值得有 trait

第一期只落地 local 文件源；etcd 作为后续里程碑的第二个真实实现，Nacos 保留同类扩展位。local 只需文件系统，etcd/Nacos 需要 gRPC、TLS、认证和长连接，因此这是一个真实的扩展点，不是一个假想的插件框架。

但仍保持 Rust 原生：

- trait 放在 `nico-config`，不放在 `nico-core`，避免核心依赖配置中心客户端。
- 每个进程只选择一个权威来源，通过本地 bootstrap 配置决定；不做多来源隐式合并。
- 若未来确需多来源，必须显式实现 `CompositeSource` 并声明优先级/覆盖/冲突规则；本地 LKG 缓存不是同级来源，不能偷偷覆盖远端配置。
- 配置源只负责加载/订阅完整文档，不解析业务路由、不直接修改运行快照。
- etcd/Nacos 实现拆 crate 并用 feature 开关，未启用时不进入 binary。

### 7.2 最小 trait 契约

以下为接口示意，不是实施代码：

```rust
// nico-config/src/source.rs（示意）
#[async_trait]
pub trait ConfigSource: Send + Sync {
    fn name(&self) -> &'static str;

    /// 返回调用时刻的完整最新文档，以及后续 watch 的起始 revision。
    async fn load(&self) -> Result<ConfigVersion, ConfigSourceError>;

    /// 从指定 revision 开始订阅；必须可取消、可有界、可重连。
    async fn watch(
        &self,
        from: Option<Revision>,
    ) -> Result<ConfigWatch, ConfigSourceError>;
}

pub struct ConfigVersion {
    pub revision: Revision,      // 来源作用域；是否可排序由来源决定
    pub digest: [u8; 32],        // 完整文档内容哈希
    pub bytes: Bytes,            // 大小受 max_bytes 限制
}

pub enum ConfigEvent {
    Snapshot(ConfigVersion),
    ResyncRequired,
}

pub type ConfigWatch =
    Pin<Box<dyn Stream<Item = Result<ConfigEvent, ConfigSourceError>> + Send + 'static>>;
```

约定：

- `Revision` 是来源作用域的不透明版本（etcd 可用 `mod_revision`）。绝不能拿 etcd revision 与 Nacos 版本比较。
- 不要求所有来源都有全序 revision。etcd 可比较 `mod_revision`；Nacos 的 MD5 只能判等，不能判先后。此时 adapter 必须在单次 watch 连接内保证不重复、不回退；server 只按 digest 去重，并用本地 generation 单调递增，不能用 MD5 排序。
- 重连后的“是否过期”由 `load` 返回的最新 `ConfigVersion` 和来源语义共同决定；无法证明新旧关系时宁可全量重载并重新编译一次。
- `watch` 只产生两种事件：`Snapshot(ConfigVersion)` 和 `ResyncRequired`。**默认不支持增量 patch**。网关配置是需要整体编译的声明式文档，部分 patch 可能产生短暂非法的中间状态。
- `watch` 的 channel 必须有界。消费者落后时发送 `ResyncRequired`，由 server 重新 `load`，而不是无限缓存。
- watch 的 Stream 被 drop 即取消订阅；实现必须 cancel-safe，不能在 drop 后继续写入或遗留后台任务。
- `load` 必须返回与 watch 起点一致的版本；不能在 load 与 watch 之间静默丢事件。
- 断线重连由 server 负责：退避 + jitter → `load` → 从新 revision 继续 watch；来源报告 compaction/gap 时强制全量重载。
- etcd 的 `start_revision` 是包含式；若 `load` 得到 revision R，watch 从 `R + 1` 开始，避免重复应用同一版本。
- trait 必须 object-safe：不使用泛型方法；async 方法按锁定 Rust 版本使用 `async_trait` 或 boxed future。

### 7.3 三种来源的边界

| 来源 | 阶段 | 实现要点 | 必须验证 |
|---|---|---|---|
| local | **第一期权威来源** | 读文件 + 大小上限 + digest + 进程内单调 revision；`notify` 目录监听与 SIGHUP；处理原子 rename、K8s symlink 切换、半写入、删除/重建 | 平台文件事件差异、重复事件、文件被删除（阶段 0 已实测覆盖） |
| etcd | 后续里程碑 | v3 API；单 key 保存完整文档；watch 从 `mod_revision + 1`；认证/TLS/endpoint failover | compaction、fragment、watch 断线、revision 回退、客户端版本行为（阶段 0 报告 §8 列恢复条件） |
| Nacos | 后续 | namespace/group/dataId；认证与 token 刷新；变更时读取完整内容 | 客户端协议版本差异、多 key 一致性、推送乱序/重复 |

`ConfigSource` 契约已由 local 实现验证，etcd 通过同一 trait 接入，不修改 reload 管线。etcd 里程碑的候选客户端为 `etcd-client` 0.21.x（etcdv3/etcd-client）：基于 tonic/prost，支持 etcd v3、Watch 和 Auth；其 Watch 是低层双向流，调用方必须自行处理 `created/canceled/compact_revision/events/fragment`。具体 API、fragment 聚合和 endpoint failover 行为须按锁定版本验证；同时必须用 `cargo tree -d` 检查其 tonic/hyper/TLS 依赖是否与 Pingora 冲突。Nacos 同样保留扩展位，不阻塞当前交付。

### 7.4 多 key 配置的一致性

强烈推荐 **一个网关集群一份完整文档**，例如 `/gateway/prod/resources.yaml`。这样 etcd/Nacos 的每次变更天然对应一个可编译快照。

etcd 里程碑默认采用单 key 完整文档；多 key 一致性是后续扩展，不作为 MVP 的前置条件。若要提前支持多 key，必须先完成全量重读和完整性门槛，再开放配置。

etcd 单个 value 受服务端 `max-request-bytes` 限制。所核对 etcd release-3.5 源码的默认值为 `1.5 * 1024 * 1024` 字节（约 1.5 MiB，目标版本仍需复核）。`bootstrap.max_bytes` 必须小于该限制，建议第一期不超过 1 MiB；不能为了塞下大配置盲目调大 etcd 限制。优先压缩路由文档，超过预算时再设计受控的多 key 方案。

如果确实要拆成 routes/upstreams/providers 多个 key，adapter 必须：

1. 任一 key 变更后重新读取**全部**相关 key。
2. 只有全部必选 key 都可读且组成完整文档时才发出 `Snapshot`。
3. 读取失败或不完整时保留 last-known-good，并报告 degraded，不能发布半个配置。
4. 不把多个 key 的独立 revision 拼成一个伪造的全局顺序；无法证明一致时发 `ResyncRequired`。

可选 key 与必选 key 必须显式声明，不能靠“缺失就当默认值”掩盖配置中心故障。

### 7.5 Bootstrap 配置不能来自配置中心

启动时必须先知道“去哪里取配置”，因此 bootstrap 只来自本地文件、环境变量或命令行参数：

```yaml
# config/bootstrap.yaml（仅本地，示意）
config_source:
  type: etcd                 # local | etcd | nacos
  endpoints: ["https://etcd-1:2379"]
  key: /gateway/prod/resources.yaml
  auth_ref: env:ETCD_AUTH     # 只存引用，不存明文凭证
  tls_ref: file:/etc/gateway/etcd-tls
  debounce_ms: 200
  max_bytes: 1048576     # 必须小于 etcd max-request-bytes；按集群配置确认
  cache_path: /var/lib/gateway/last-good.yaml
  allow_bootstrap_cache: false
```

这份 bootstrap 只包含连接、来源选择、缓存和资源上限，不包含路由。它应由部署系统管理，不能让远端配置中心自行改写“去哪里读配置”或本地文件路径。

### 7.6 启动、故障与降级

- **启动无可用快照**：默认不 ready，不允许带着空路由接流量。只有显式 `allow_bootstrap_cache: true` 时才用本地 last-known-good 启动，并标记 `stale`。
- **运行期配置中心不可用**：继续使用 last-known-good 服务，标记 `config_degraded`，按阈值告警；watch 退避重连，不阻塞请求。
- **etcd key 被删除**：视为配置不可用，不是“空配置”；启动阶段不 ready，运行期保留 LKG 并告警，不能把删除事件编译成空路由。
- **etcd watch fragment**：大 revision 可能被拆成多个 `WatchResponse`；adapter 必须按 watch_id 聚合到 `fragment = false`，或显式拒绝并触发全量重载，不能把半个 value 当成完整配置。
- **etcd 认证失败/权限不足**：启动阶段不 ready；运行期保留 LKG 并告警，不能把权限错误当作空配置。
- **新配置编译失败**：保留 LKG，不反复热循环；记录 revision、digest 和错误类别，等待下一次变更或人工回滚。
- **配置风暴**：去抖 + 最大等待 + single-flight + digest 去重；同一时刻最多一个编译任务。
- **revision 乱序/回退**：同一来源内比较 revision，旧事件丢弃；来源无法保证顺序时走全量 `load`。
- **回滚**：保留上一份已编译快照的元数据；通过配置中心回退到旧 revision，或由本地受控管理接口触发显式 reload。不把远端配置里的任意路径当作回滚命令。

### 7.7 安全与资源限制

配置中心是高权限输入，必须按不可信边界处理：

- TLS/mTLS、认证、namespace/RBAC 和最小读取权限。
- 限制文档大小、解析深度、路由数量、upstream 数量和编译时间，防止配置中心故障或恶意内容打爆内存。
- 严格 schema；未知字段默认拒绝，不能静默忽略导致“配置看起来生效”。
- 禁止配置携带任意本地文件读取、命令执行或动态库路径；secret 只保存引用。
- 日志与指标只记录 source、revision、digest、耗时和错误类别，不记录完整配置或 secret。
- 可选对文档做签名/摘要校验，特别是跨公网或多人可写的配置中心。

### 7.8 可观测性与测试

指标至少包括：`config_load_total{source,result}`、`config_reload_total{result}`、`config_compile_seconds`、`config_watch_reconnects_total`、`config_snapshot_age_seconds`、`config_degraded`，以及当前 revision/digest。

测试使用 `FakeConfigSource` 覆盖：

- 首次加载、重复 digest、乱序 revision、revision 回退。
- watch 断开、compaction/gap、慢消费者触发 `ResyncRequired`。
- 超大文档、非法 UTF-8/YAML、未知字段、引用缺失。
- 编译失败保留 LKG、随后修复成功发布。
- 配置中心不可用期间在途 SSE/H2 不受影响。
- 配置风暴下去抖与 single-flight 生效，无 reload 抖动。

etcd 里程碑使用固定版本 etcd 容器做集成测试，验证认证、TLS、watch 重连、compaction 和 endpoint failover；不能用 fake 测试替代真实客户端验证。Nacos 集成测试在其后续实现时补充。当前 local-first 阶段已用 fake + local 覆盖去抖、single-flight、LKG 与乱序/重复场景。

## 8. 流式与异步的 Rust 实现取向

- Pingora 拥有其运行时和 HTTP I/O，不另建第二个 Tokio runtime。
- body/stream 使用 `Bytes`/`BytesMut` 和具体 Stream 类型，避免 `String` 复制。
- SSE 使用 `memchr` 定位换行；不完整 remainder 由 per-stream `BytesMut` 保存。
- `Provider` 创建每次响应独立的 `ProviderStream`，不在 Provider 单例中保存解析器。
- 用 `tokio_util::sync::CancellationToken` 或 Pingora 对应机制传播取消。
- 用明确 timeout/deadline 限制整个上游调用，不只限制单次 read。
- 不用无界 `tokio::mpsc` 把 Filter 串成 actor 系统；优先直接调用和背压。
- `thiserror` 用于库错误分类，`anyhow` 只考虑用于 binary 启动边界；错误最终由 HTTP/AI 层映射为协议响应。
- 使用 `tracing` span 关联 request、provider call 和 retry；不自建通用观测插件框架。

## 9. Cargo Feature 与部署角色

Feature 用于**编译期依赖裁剪**，配置用于**运行时行为**：

- `default`：HTTP + 内置策略 + local 配置源。
- `ai`：加入 `nico-ai`。
- `builtin-plugins`：加入 JWT/API Key/Rate Limit。
- `config-etcd`：加入 `nico-config-etcd`，etcd 里程碑部署启用。
- `config-nacos`：后续里程碑启用，第一期不创建实现 crate。
- 后续可选：`mqtt`、`quic`、`wasm-plugins`。

Cargo feature 只决定哪些配置源实现被编入；`bootstrap.config_source.type` 在运行时选择唯一权威来源。允许一个 binary 同时编入 local/etcd/Nacos，但不能同时激活多个权威来源。默认保留 local 作为文件模式和 LKG 缓存能力，etcd/Nacos 客户端依赖只在启用对应 feature 时编入。

建议产物：

- 开发：单个 all-in-one binary。
- 生产：同一 Workspace 构建 traffic 与 ai 两种 feature/role 组合，独立部署。
- 未来 MQTT/QUIC：可新增 binary target 或独立 crate，不强迫经过 Pingora HTTP Filter。

Feature 是 additive 的，不能依赖“启用 A 后悄悄改变 B 的非 feature 行为”来做复杂互斥；角色与路由合法性仍需启动时校验。

## 10. 四个扩展示例

### 新增 AI Provider

1. 在 `nico-ai::provider` 新增模块和强类型 Config。
2. 实现 `Provider` 与 `ProviderStream`。
3. 在 server 的配置 builder 注册类型名。
4. 不修改 `nico-core` 和普通 HTTP Filter。

### 新增认证插件

1. 在 `nico-plugins` 新增模块。
2. 实现 `nico-http::HttpFilter`。
3. 在 server 注册 builder，由路由引用。
4. 不修改 AI Provider 和 upstream 实现。

### 新增 MQTT 数据面

1. 新建独立 MQTT crate/binary，定义 MQTT 自己的 packet/session trait。
2. 只复用 `nico-core` 中真正通用的 config/context 组件。
3. 不让 MQTT Packet 适配成 Pingora Session，也不复用 HTTP Filter。

### 新增配置中心

1. 在独立 crate 中实现 `nico-config::ConfigSource`。
2. 将客户端协议错误映射为 `ConfigSourceError`，将版本映射为来源内 `Revision`。
3. 在 `nico-server::bootstrap` 的 `match source_type` 中注册。
4. 不修改 reload 管线、路由编译和 HTTP/AI 代码。

## 11. 动态插件是后续里程碑，不属于第一期内核

第三方无需重编译、无需重启主程序即可交付插件是**已确认的需求**，但它属于后续里程碑，不在第一期：

- 新增独立 `nico-wasm` crate，用 WASM 沙箱承载受限的插件逻辑；高风险或重依赖能力可另用独立进程 + 有界 IPC。
- 动态 artifact 的版本、签名、权限、资源限制与沙箱预算由该扩展层负责，**不反向污染核心**：`nico-core`、
  `nico-http` 不为它改接口，路由与策略编译不为它加分支。
- 插件系统只有一套：内置插件（Rust，随二进制编译）与自定义插件（WASM，独立 artifact）都是配置里同一个
  具名集合的条目，`kind` 决定实现来源；核心只校验形状与引用，某个 kind 的阶段、失败映射与沙箱预算由拥有它的
  层负责，所以加 `wasm` kind 时核心不认识 wasm 运行时。
- 不把 Rust dylib 作为方案，也不提前为它设计 ABI；宿主 ABI 随该里程碑的 PRD 定义。

## 12. 阶段计划与验收

### 阶段 0：Workspace 与 local 配置源（已完成）

- 已建立 `nico-core` / `nico-config` / `nico-server` 的依赖边界；`nico-core` 不依赖协议或来源实现。
- 已定义 `ConfigSource`、`Revision`（含 `Comparable` / `EqualityOnly`）、`ConfigDigest`、去抖与 single-flight 发布管线。
- 已用 `FakeConfigSource` 与 local 文件源覆盖重复 digest、乱序/回退、慢消费者、编译失败保留 LKG、stale 启动、在途快照不受影响。
- 已实测 Pingora `0.9.1` 的 H1/H2、body、SSE、背压、取消、连接池、graceful 与 WebSocket/gRPC 边界（证据见已归档任务 `archive/2026-11/11-03-phase0-tech-verification`）。
- 尚未实现 HTTP 数据面：`nico-http` 在阶段 1 建立。

### 阶段 1：静态组合 MVP（local 先行）

- HTTP 路由、JWT/API Key/Rate Limit、upstream、普通 API；`nico-http` 首次落地 Pingora 集成。
- AI Filter、OpenAI Provider、SSE 透传。
- 普通与 AI 路由共享 HTTP/Core，但使用不同 CompiledRoute。
- 配置来源使用 **local 文件**（`ConfigSource` 唯一实现）；etcd 作为后续里程碑接入，不改契约与管线。
- **路由不依赖 body**：provider/upstream 由 vhost、path 与路由级 `provider` 字段决定；不实现"读请求 body 再选上游"。
  依据：Pingora 过滤器读完 body 不会重放，上游会挂起（阶段 0 实证）。
- "有界缓冲 + 单次重放"（按 `model` 路由）是独立的后续设计项，不进入阶段 1：它需要先定义重放语义、
  缓冲上限、压缩 body 与超限拒绝规则，并单独验收。
- 阶段 1 必须按 Pingora 实测结论实现：明文 H2 显式 `h2c`、按流式状态机处理 body、显式配置 graceful 宽限期、1xx 不插入响应头、gRPC 路由显式 `ALPN::H2`。
- Nacos 保持 `ConfigSource` 扩展位，不进入第一期验收。

### 阶段 2：Provider 与长寿命状态

- Anthropic Provider/Stream、usage、限额、熔断和安全重试。
- SharedServices 与配置 generation 的继承/drain 规则。
- traffic/ai 两种部署形态。

### 阶段 3：按真实需求扩展

- 新协议独立 crate/binary，或新增 WASM/进程扩展层。
- 只有第二个真实消费者出现时才继续抽象公共 trait。

### 架构验收

- `nico-core` 的依赖树没有 Pingora、HTTP、AI 或具体插件。
- 新增 Provider 只需修改 AI 模块与 server 注册点。
- 普通 API 路由没有 AI 状态初始化和模型分支。
- 热路径没有插件名字符串查找、通用事件总线或 JSON 中转。
- 一次 reload 只构建一次路由链；失败不影响旧快照。
- 配置源乱序/重复/断线/gap 不会发布错误快照；配置中心故障期间请求继续使用 LKG。
- etcd watch 从 `mod_revision + 1` 续订，compaction 后强制全量重载；fragment 被正确处理；endpoint failover 和认证/TLS 通过集成测试。
- `etcd-client` 锁定版本的 Watch/Auth/TLS API 通过编译验证；`cargo tree -d` 不出现与 Pingora 冲突的 tonic/hyper/TLS 栈。
- `nico-core` 与默认 binary 的依赖树没有 etcd 客户端；Nacos 客户端在后续实现前不存在。
- `cargo check/test/clippy` 在不同 feature 组合下通过。
- 全 Workspace 不存在为了“看起来像插件系统”而增加的空 trait、空 Manager 或单实现微 crate。

## 13. 当前结论

当前架构的核心是：

> **Cargo Workspace + 6 个初始 crate（+2 个可选配置源 crate）+ crate 内模块 + 领域 trait + 启动时显式组装 + CompiledRoute + ArcSwap。**

配置来源使用 `ConfigSource` trait，第一期只落地 local 文件源，etcd 与 Nacos 作为后续里程碑通过同一 trait 接入；它只有“加载 + 订阅完整文档”职责。revision 排序、去抖、single-flight、校验、编译和发布仍集中在 `nico-server` 的一条管线中。

第三方动态插件、`quick` 的准确含义、第一期 SLA 和 Provider 清单仍是独立后续决策，不影响这套 Rust 原生结构。

HTTP/AI 正确性与协议边界继续沿用：

- `research/http-ai-implementation-review.md`
- `research/protocol-runtime-summary.md`
- `research/protocol-and-runtime-boundaries.md`
