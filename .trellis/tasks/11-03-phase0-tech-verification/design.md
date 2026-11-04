# 阶段 0 技术设计：本地配置源骨架与 Pingora 能力验证

> 本文描述本任务最终要落地的结构与契约。架构整体背景见父任务 `design.md`；本阶段的关键变化是**第一期配置权威来源为本地文件，etcd 推迟**。

## 1. 产物边界

### 1.1 长期保留（真实骨架）

```text
nico/
├── Cargo.toml               # [workspace] members = ["crates/*"], workspace deps + lints
├── Cargo.lock               # 提交
├── rust-toolchain.toml       # 1.99.0
├── rustfmt.toml              # night-only 选项依赖 nightly 工具链
├── clippy.toml               # msrv + 测试内 unwrap/expect 允许
├── deny.toml                 # advisories / licenses / bans
├── config/
│   ├── bootstrap.yaml       # 来源选择、路径、上限、去抖、缓存开关（仅本地）
│   └── resources.yaml        # 网关资源文档（本阶段只有 apiVersion + limits）
└── crates/
    ├── gateway-core/src/     # config.rs（Document/Limits/Generation/Revision/ConfigDigest）、
    │                         # snapshot.rs（RuntimeSnapshot/SnapshotHandle）、status.rs、
    │                         # error.rs（ErrorCategory）
    ├── gateway-config/src/   # source.rs（契约）、local.rs、stream.rs（有界可取消 Stream）、
    │                         # cache.rs、fake.rs、lib.rs
    └── gateway-server/src/   # main.rs（薄）、lib.rs、bootstrap.rs、
                              # reload.rs（订阅循环 + 去抖 + single-flight + 发布）
```

### 1.2 一次性（验证后删除）

```text
poc/                          # 独立 workspace，仅 Pingora 行为取证
├── Cargo.toml
├── rust-toolchain.toml
├── README.md                 # 一次性资产声明
└── spikes/src/{lib.rs,bin/{spike-http.rs,spike-mock.rs}}
```

### 1.3 本阶段不创建

`gateway-http`、`gateway-ai`、`gateway-plugins`、`gateway-config-etcd`、`gateway-config-nacos`、metrics exporter、CI。

`gateway-http` 之所以不建：Pingora 的 body hook、flush、连接池语义尚在取证，先建 crate 会把未验证形状固化成契约。取证通过后由阶段 1 任务建立。

## 2. 版本与工具链

| 组件 | 锁定 | 依据 / 备注 |
|---|---|---|
| Rust | `1.99.0`（stable aarch64-apple-darwin），`rust-toolchain.toml` | 本机已存在；夜间版另装用于 fmt |
| edition / resolver | `2024` / `3` | 1.85+ 可用；与 spec 示例的 2021 差异记录在报告 |
| MSRV | `1.85` | 由 `tokio-util 0.7.20`、`notify 8.2.1`、edition 2024 取上界 |
| `tokio` | `=1.54.1`，features `rt-multi-thread,fs,signal,sync,time,macros` | 需要 signal 处理 SIGHUP |
| `tokio-util` | `=0.7.20` | `CancellationToken` |
| `bytes` | `=1.12.1` | 文档字节用 `Bytes` |
| `arc-swap` | `=1.9.3` | 快照发布 |
| `serde` | `=1.0.229` | derive |
| YAML | `serde_norway = "=0.9.42"` | `serde_yaml` 的维护分支，MSRV 1.71 |
| `sha2` | `=0.10.9` | 0.11 是新主版本，无收益 |
| `notify` | `=8.2.1` | 9.0.0-rc.7 是预发布 |
| `async-trait` | `=0.1.92` | dyn 兼容的 async trait |
| `thiserror` | `=2.0.21` | 分层错误 |
| `tracing` / `tracing-subscriber` | `=0.1.x` | 结构化日志 |
| `proptest` | dev-dependency | 解析器健壮性 |

被拒绝的候选及理由：

| 候选 | 拒绝原因 |
|---|---|
| `serde_yaml` | 上游归档，无安全修复 |
| `serde_yml 0.0.13` | `0.0.x` 版本线，分叉质量与兼容性无法确认 |
| `saphyr 0.3.3` | 纯解析器，需要手写类型映射，收益不足 |
| `notify 9.0.0-rc.7` | 预发布 |
| `sha2 0.11.1` | 新主版本，本场景无收益 |
| `rustls 0.24.0-dev.2` | 预发布 |
| `etcd-client 0.21.1` | 本阶段不接入 etcd，不锁定 |
| `pingora` | 仅 `poc/` 使用，不进入产品 workspace |

工具链门禁偏差处理：若 `rustup toolchain install nightly` 失败，则关闭 `rustfmt.toml` 中依赖 nightly 的 `group_imports`/`imports_granularity`，改用 stable `cargo fmt --check`，并在报告中记录偏差。

## 3. 依赖方向

```text
gateway-server
   ├──> gateway-core
   └──> gateway-config ──> gateway-core

gateway-core: serde, serde_norway, sha2, thiserror, arc-swap
gateway-config: gateway-core, async-trait, tokio, tokio-util, bytes, notify, thiserror, tracing
gateway-server: gateway-core, gateway-config, tokio, tokio-util, arc-swap,
                serde, serde_norway, thiserror, tracing, tracing-subscriber, anyhow(main only)
```

硬规则：

- `gateway-core` 不得依赖 `pingora`、`gateway-http`、`gateway-ai`、`gateway-config`、`notify` 或任何配置中心客户端。它允许依赖 `serde`/`serde_norway`/`sha2`/`arc-swap`，因为这些是**数据模型与快照发布**，不是协议或传输。
- `gateway-config` 不得依赖 `gateway-server`，不得把 `notify::Event`、etcd 类型泄漏到公开签名。
- `gateway-server` 是唯一知道具体来源实现（`LocalFileSource` / `FakeConfigSource`）的 crate。
- 代码归属判定沿用 spec：文档模型与快照 → core；来源协议 → config；编排 → server。

## 4. `ConfigSource` 契约

```rust
// gateway-config/src/source.rs（示意，实施时以编译通过为准）
//
// 注意：`Revision`、`RevisionOrdering`、`ConfigDigest` 实际定义在 `gateway-core::config`，
// 由 `gateway-config` 重导出。原因：`RuntimeSnapshot` 位于 core 且需要引用它们，若把它们放在
// config 就会形成 core → config 的反向依赖。
#[async_trait]
pub trait ConfigSource: Send + Sync {
    /// 来源名称，用于日志与错误信息。
    fn name(&self) -> &'static str;

    /// 读取调用时刻的完整文档及与其一致的 revision。
    async fn load(&self) -> Result<ConfigVersion, ConfigSourceError>;

    /// 从 `from` 之后订阅变更；drop 即取消。只发完整快照或 `ResyncRequired`。
    async fn watch(&self, from: Option<Revision>)
        -> Result<ConfigWatch, ConfigSourceError>;
}

pub struct ConfigVersion {
    pub revision: Revision,
    pub bytes: Bytes,
}

pub struct Revision {
    seq: u64,
    ordering: RevisionOrdering,
}

#[non_exhaustive]
pub enum RevisionOrdering {
    /// 同来源内可比较先后（etcd mod_revision、本地进程内计数器）
    Comparable,
    /// 只能判等（内容摘要类版本）
    EqualityOnly,
}

#[non_exhaustive]
pub enum ConfigEvent {
    Snapshot(ConfigVersion),
    ResyncRequired,
}

pub type ConfigWatch =
    Pin<Box<dyn Stream<Item = Result<ConfigEvent, ConfigSourceError>> + Send + 'static>>;
```

契约语义（每条都要有对应测试或文档说明）：

1. `load` 返回的 revision 与随后 `watch(from = Some(revision))` 的起点一致；来源若从 `revision` 开始而非之后开始，由 adapter 自行加一（etcd 的包含式语义在这里被吸收）。
2. `watch(None)` 表示「从现在开始」，用于首次启动后直接订阅。
3. 只允许两种事件；不提供 patch。任何需要局部更新的来源都必须自行合成完整文档或发 `ResyncRequired`。
4. stream 输出有界；消费者落后时 adapter 合并事件并发 `ResyncRequired`，不缓存无限历史。
5. drop stream 即取消：不得遗留后台任务、文件监听器或未关闭的连接。
6. 来源故障映射为 `ConfigSourceError`，不允许把故障表达为空文档或零字节快照。
7. `digest` 由管线用 `gateway_core::config::ConfigDigest::of(&bytes)` 统一计算（`gateway-config` 重导出该类型），adapter 不各自实现哈希，避免算法漂移。

### 4.1 etcd 可接入性检查表（本阶段用 local + fake 间接验证）

| 契约要求 | 本阶段如何验证 | 恢复 etcd 时仍需实测 |
|---|---|---|
| revision 来源作用域、可声明有序性 | fake 构造 `Comparable` 回退 / `EqualityOnly` 乱序 | `mod_revision` 语义与回退 |
| 仅完整快照事件 | fake 与 local 都只发 Snapshot | fragment 聚合 |
| 不从来源泄漏类型 | 编译期检查公开签名不含来源类型 | — |
| 故障不等于空配置 | fake 注入 `Unavailable` | key 删除、认证失败 |
| 有界 + 可取消 | fake 慢消费者触发 `ResyncRequired` | watch 断线重连、compaction |
| 摘要集中计算 | 单一 `ConfigDigest::of` 调用点 | — |

## 5. 文档模型（最小可用）

```yaml
# config/resources.yaml
apiVersion: gateway/v1
limits:
  max_request_body_bytes: 1048576
  request_timeout_ms: 30000
```

- `Document { api_version, limits }`，`#[serde(deny_unknown_fields)]`，文档级结构不使用 `serde(default)`。
- `Limits { max_request_body_bytes: u32, request_timeout_ms: u32 }`，解析后校验区间 `1..=8 MiB`、`1..=600_000 ms`，越界报 `Limit` 类错误。
- `apiVersion` 必须等于 `gateway/v1`，否则 `Client` 类错误。
- 本阶段**不含** `routes`/`upstreams`/`providers`：参考校验阶段随阶段 1 的路由 schema 一起落地，先定义 schema 会把未设计的路由 DSL 固化成契约。
- 解析入口 `Document::parse(&[u8], ParseLimits) -> Result<Document, DocumentError>`，`ParseLimits` 本阶段只含 `max_bytes`（沿用 bootstrap 上限）；嵌套深度与节点数上限随阶段 1 的路由 schema 一起实现，YAML 别名膨胀风险记入报告。

## 6. 本地文件源

配置项（来自 bootstrap）：`path`、`max_bytes`、`watch`（`notify` | `signal` | `notify+signal`，默认 `notify+signal`）。

行为规则：

| 场景 | 行为 |
|---|---|
| 正常读取 | 取元数据大小 → 读入有界 buffer → 复查实际长度 ≤ `max_bytes` → 返回 `ConfigVersion`（本地进程内单调 seq、`Comparable`） |
| 超限 | `ConfigSourceError::TooLarge`，不读取全部内容 |
| 非法 UTF-8 | `ConfigSourceError::NotUtf8`（由管线在解析阶段判定，adapter 只做长度与可选 UTF-8 预检） |
| 原子 rename 替换 | 监听**配置路径所在目录**并按路径过滤，事件后重新 `load`；不持有已打开的旧 inode |
| K8s symlink 切换 | 同上：订阅目录事件，每次读取都重新解析配置路径，`..data` 切换可被观测 |
| 半写入 | 读取到不完整内容仍可能成功读入，但后续解析失败 → 管线保留 LKG；adapter 不负责猜测完整性 |
| 文件删除 | 发 `Err(Unavailable)`，保持监听以便文件重建后恢复 |
| 文件重建 | 再次 `load` 成功后发 `Snapshot` |
| `SIGHUP`（仅 Unix） | 触发一次 `load`，与 notify 事件同一路径；`#[cfg(unix)]` |
| 重复/抖动事件 | adapter 不合并，交给管线按 digest 去重（避免两处实现同一策略） |

实现约束：

- 用 `tokio::sync::mpsc::channel(capacity=8)` + 自定义 `Stream` 包装（不引入 `tokio-stream`）；`try_send` 满时置 `pending_resync = true`，下一次成功发送时改为发 `ResyncRequired`。
- `notify` 的 `RecommendedWatcher` 在专用阻塞线程/任务中运行，事件经有界通道送入；流 drop 时 watcher 与任务一并退出。
- 不实现轮询模式：多一个实现即多一套语义，文件系统事件不可用时以 `Unavailable` 暴露问题而不是静默降级。

## 7. LKG 缓存

- 文件布局：`<cache_dir>/last-good.yaml`（文档原字节）+ `<cache_dir>/last-good.meta.yaml`（`revision_seq`、`digest_hex`、`generation`、`written_at_unix_secs`、`source`）。元数据同样用 YAML，避免为一个元数据文件再引入 JSON 依赖。
- 写入：同目录临时文件 → `write_all` → `sync_all` → `rename` → （best effort）目录 `sync_all`。
- 读取：校验 meta 可解析、`digest_hex` 与文档字节重算一致、`generation > 0`；任一不满足即视为损坏。
- 使用条件：仅当 `bootstrap.allow_bootstrap_cache: true` 且启动 `load` 失败时；发布后 `stale = true`，并在状态中保留 `last_error_category`。
- 缓存写失败：`warn` 日志 + 计数，不影响已发布快照与就绪状态。
- local-first 下缓存价值有限（配置文件本身在本地），它主要在「文件被删除后受控重启」与后续远端来源迁移时使用；默认关闭。

## 8. 发布管线

状态机（`gateway-server/src/reload.rs`）：

```text
Start
  ├─ load() 成功 ──> compile ──> publish(generation=1) ──> Watching
  ├─ load() 失败 + allow_bootstrap_cache ──> cache 可用且完好 ──> publish(stale) ──> Watching
  └─ load() 失败且无缓存 ──> NotReady（重试退避）

Watching
  ├─ Snapshot(v)   ──> 去重（digest / 乱序）──> 去抖 ──> single-flight compile ──> publish  | 失败保留 LKG
  ├─ ResyncRequired ──> load()（全量）──> 同上
  └─ Err(e)        ──> 标记 degraded、按类别告警、退避后 load()
```

规则：

- **去抖与最大等待**：`debounce_ms=200`、`max_wait_ms=1000`，`tokio::time` 实现，测试用 `pause()` + `advance`。持续变更不得让发布无限延后。
- **single-flight**：编译任务最多一个；其间到达的事件只保留最新一份候选（覆盖而非排队）。
- **去重与排序**：digest 与当前快照相同 → 跳过编译并记录；`Comparable` 且 `seq <= current` → 丢弃；`EqualityOnly` → 只依赖 digest 去重与 adapter 单连接顺序。
- **发布**：先构建完整 `RuntimeSnapshot { generation, revision, digest, document, source }`，成功后 `SnapshotHandle::store`；`generation = previous + 1` 单调递增，回退与乱序都不会使其减小。
- **失败**：保留 LKG、`degraded = true`、记录 `ErrorCategory`，不针对同一文档循环重试（等待下一次事件或周期性 `load` 退避）。
- **就绪**：`ready` 仅在首次成功发布后为 `true`；`stale` 只在缓存启动时为 `true`。
- **停机**：`CancellationToken` 取消 watch 与编译任务，等待任务 join；数据面持有的 `Arc<RuntimeSnapshot>` 不受影响。
- **可观测**：`ConfigStatus { generation, revision, digest, ready, stale, degraded, last_error_category, last_reload_unix_secs }` 通过 `SnapshotHandle::status()` 读取；`tracing` 事件字段固定为 `source`、`generation`、`revision_seq`、`digest`、`size`、`error_category`。不引入 metrics exporter（deferred）。

并发正确性：`SnapshotHandle` 内部是 `ArcSwap<RuntimeSnapshot>`，读方 `load_full()` 一次拿到 `Arc`，整段逻辑使用同一 generation。

## 9. Pingora PoC（`poc/`）

结构与判定与阶段 0 原方案一致，去掉 etcd 部分：

| 能力 | 判定问题 | 手段 | 目标等级 |
|---|---|---|---|
| H1/H2 | 能否同时服务，H2 客户端优先 | `curl --http1.1` 与 `--http2-prior-knowledge` 命中并记录协议版本 | L2 |
| body 钩子 | 是否按块可见、能否无损转发 | 钩子记录块数与长度，mock 校验总字节与顺序 | L2 |
| 元数据晚到 | body 读完前是否已连上游 | 首次 body 块与上游连接建立时刻打点 | L2 |
| SSE flush | 事件级 flush 是否可控 | mock 每 200ms 发一个事件，记录首事件时延与节奏 | L2 |
| 慢客户端背压 | 上游是否被反压、内存是否有界 | 上游超量发送 + 客户端慢读，记录阻塞点与内存趋势 | L2 |
| 取消传播 | 客户端断开是否取消上游 | 客户端 100ms 断开，mock 记录连接关闭时刻 | L2 |
| 连接池作用域 | 池归属与复用可见性 | 连续请求打印 mock 侧 local port | L2 |
| graceful 退出 | 在途请求/长连接 drain | SSE 进行中触发停机 | L2 |
| WebSocket / gRPC | 支持的代理模式边界 | Upgrade 后发帧；裸 H2 + trailers 最小帧 | L2（失败则 L0 + 未验证） |

`spike-mock` 用手写 `tokio::net::TcpListener`，不引入 hyper/axum，避免污染依赖树结论。

## 10. 测试设计

| 层 | 覆盖 |
|---|---|
| `gateway-core` | 解析成功/未知字段/错误 `apiVersion`/limits 越界/超限/非 UTF-8/空文档；`ConfigDigest::of` 稳定性；proptest：任意字节不 panic；`ErrorCategory` 映射 |
| `gateway-config` | fake：重复 digest、`Comparable` 回退、`EqualityOnly` 乱序、慢消费者触发 `ResyncRequired`、注入 `Unavailable`；local（`tempfile`）：原子 rename、symlink 切换、半写入、删除/重建、超限、SIGHUP；cache：原子写、meta 校验、digest 不匹配、写失败不 panic |
| `gateway-server` | 管线（`tokio::time::pause`）：去抖、最大等待、single-flight、digest 去重、失败保留 LKG、修复后发布、not-ready、stale 启动、`generation` 单调；并发：旧 `Arc` 内容不变；停机：取消后无遗留任务 |
| `poc/` | Pingora 九项 L2 实测（独立 workspace，不进产品门禁） |

测试约定沿用 spec：行为命名、错误也要断言、异步用 `current_thread` + `pause`、禁止 `sleep` 等待。

## 11. 报告结构

`research/phase0-capability-report.md`：

1. 结论摘要（每项一行 + 结论枚举 + 证据等级）
2. 版本矩阵（含拒绝项与工具链偏差）
3. `ConfigSource` 契约就绪度（§4.1 检查表逐行结论）
4. 本地源验证（场景 → 测试名 → 观察结果）
5. 发布管线验证（规则 → 测试名 → 观察结果）
6. Pingora 能力表（九项）
7. 依赖边界与构建证据（`cargo tree` 摘要、五条基线输出摘要）
8. **deferred**：etcd 恢复时必须重新取证的 6 项（Watch 包含式 revision、compaction、fragment、Auth/TLS、endpoint failover、`max-request-bytes` 与依赖冲突），以及契约是否需要扩展的判定标准
9. 对父任务的修正建议（`原表述 → 实测 → 建议改写`）
10. 复现步骤

## 12. 回写规则

- 父任务 `design.md`：把「第一期 etcd 为生产权威来源」校正为 local-first，etcd 移入后续里程碑；`§7.3` 表格与 `§12` 阶段划分同步。该改动在报告完成后执行并经评审。
- 父任务 `implement.md`：勾选已被技术关闭的决策门（版本锁定、依赖边界、契约、local 源、管线、Pingora 能力），并改写 etcd 相关条目为 deferred。
- 父任务 `research/protocol-runtime-summary.md`：更新「版本证据边界」与配置源约束中与 local-first 冲突的表述。
- `poc/` 代码不迁移；报告引用其路径并注明一次性。

## 13. 风险与处置

| 风险 | 处置 |
|---|---|
| `nightly` 工具链或 `cargo-deny` 安装失败 | 使用 documented fallback（stable fmt + 记录偏差）；`deny.toml` 仍提交 |
| `notify` 在 macOS FSEvents 上合并/延迟事件 | 不依赖事件数量，靠 digest 去重；测试用显式触发（写文件 + 等待事件 + 超时断言） |
| symlink 切换测试在 macOS 上差异 | 用 `tempdir` 构造真实 symlink 切换；失败则记录平台差异并降级为「目录内 rename」覆盖 |
| YAML fork 生态（`serde_norway`）后续停更 | 解析入口集中在 `Document::parse`，替换 crate 只改一处；报告中记录候选与替换成本 |
| 无 metrics exporter 导致观测不足 | 以 `ConfigStatus` + 结构化日志覆盖本阶段；指标落地列入 deferred |
| 骨架先行导致未验证形状被固化 | 骨架只包含已验证语义（契约、local 源、管线），Pingora 相关代码一律留在 `poc/` |
