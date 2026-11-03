# 后续 Rust 实施计划（仅供评审，当前不执行）

> 当前任务只交付架构分析。本文件描述未来获得单独实施授权后的建议顺序；本次不得创建 Cargo/Rust 产品代码，也不得据此运行 `task.py start`。WebRTC 暂不纳入范围。

## 1. 前置决策门

- [ ] 确认 `quick` 是否指 QUIC，以及目标是 HTTP/3、QUIC proxy 还是 WebTransport；未确认前不进入 MVP。
- [ ] 确认第一期 AI provider、模型兼容范围、最大请求体、SLA 和部署环境。
- [x] 第一期配置中心确定为 etcd；Nacos 推迟，保留 `ConfigSource` 扩展位。
- [ ] 确认 etcd 采用“一个集群一份完整文档”还是多 key；默认单 key 完整文档，多 key 不作为 MVP 前置条件。
- [ ] 确认启动时配置中心不可用的策略：默认 not ready；是否允许受控使用 last-known-good 缓存启动。
- [ ] 锁定 Rust 工具链、Pingora、TLS、异步运行时、配置中心客户端版本，形成源码与 PoC 证据。
- [ ] 第一版默认静态链接和显式注册；只有第三方动态安装成为真实需求时才另建 WASM/进程扩展任务。

## 2. 阶段 0：Workspace、配置源与 Pingora PoC

- [ ] 建立 6 个初始 crate：`gateway-server/core/http/ai/config/plugins`，Provider 先作为 AI 内部模块。
- [ ] 用 Cargo 依赖约束保证 Core 不依赖 Pingora、HTTP、AI 或具体插件。
- [ ] 在 `gateway-http` 定义最小 HTTP Filter/Route 接口，不创建通用 SPI crate。
- [ ] 在 `gateway-config` 定义 `ConfigSource`、revision/digest、错误和 `FakeConfigSource`。
- [ ] 实现 local 文件源：大小上限、digest、SIGHUP/可选 notify、原子 rename 和半写入处理。
- [ ] 实现订阅管线：去抖、最大等待、single-flight、revision 排序、digest 去重、LKG 和 degraded 指标。
- [ ] 用 FakeConfigSource 验证乱序、重复、断线、gap、慢消费者、编译失败和回滚。
- [ ] 验证 local 原子替换、K8s symlink 切换、文件删除/不可读。
- [ ] 评估并锁定 `etcd-client` 0.21.x：验证 Watch API、fragment、Auth、TLS feature、MSRV 和 endpoint failover。
- [ ] 运行 `cargo tree -d`，确认 `etcd-client` 的 tonic/prost/hyper/TLS 依赖不与 Pingora 冲突。
- [ ] 验证 etcd 客户端最小能力：认证、TLS、endpoint failover、`mod_revision` 读取和 watch 建立。
- [ ] 用两个手写 Filter 验证 CompiledRoute、静态 builder 和 ArcSwap reload。
- [ ] 验证 Pingora H1/H2、body hooks、SSE、背压、取消、连接池和长连接语义。

**评审门**：没有 PluginRegistry 容器、Manifest 框架、通用 Lifecycle Manager 或单实现微 crate；热路径无字符串查找；配置源不能直接修改运行快照。

**回滚点**：若抽象需要 Core 依赖具体协议，将 trait 移回领域 crate，而不是建立更多 SPI 层。

## 3. 阶段 1：静态组合 HTTP + AI MVP

- [ ] `gateway-server` 显式构造 Filter、Provider、Upstream 和 SharedServices。
- [ ] 使用 serde 强类型配置构建 RouteTable/CompiledRoute，并通过 ArcSwap 发布。
- [ ] 实现普通 API 路由、JWT/API Key/Rate Limit 和 HTTP Upstream。
- [ ] 实现 AI Filter、OpenAI Provider 模块和有界 body 转发。
- [ ] 实现 SSE 透传；WebSocket/gRPC 只承诺 PoC 已覆盖的代理模式。
- [ ] 实现响应提交前的有限重试，禁止流开始后的透明 fallback。
- [ ] 接入 etcd 作为第一期唯一实时权威来源，验证认证、TLS、watch 重连、乱序/重复推送和故障降级。
- [ ] 验证 etcd watch 的 `start_revision` 为包含式：从 `mod_revision + 1` 续订，避免重复应用。
- [ ] 验证 compaction/gap 时 watch 被取消并带 `compact_revision`，随后强制全量 `load`。
- [ ] 验证 fragment 响应按 watch_id 聚合，或显式拒绝并触发全量重载。
- [ ] 验证 etcd endpoint failover、watch 断线退避重连和配置中心不可用期间的 LKG 服务。
- [ ] 验证 key 删除、认证失败、权限不足不会发布空配置，保留 LKG 并标记 degraded。
- [ ] 默认使用单 key 完整文档；多 key 全量重读与完整性门槛推迟到 etcd 稳定后。
- [ ] 配置变化走热重载；代码和 feature 变化走滚动发布。
- [ ] Nacos 不作为第一期验收项；在 etcd 稳定后按独立任务实现。

**评审门**：新增 Provider 不改 Core/HTTP 路由；普通 API 无 AI 状态；任意分片不丢字节；reload 失败不影响旧流量；配置源乱序/断线/gap 不发布错误快照。

**回滚点**：回滚配置 generation 或静态 artifact，不引入动态加载器解决发布问题。

## 4. 阶段 2：Provider 与长寿命状态

- [ ] 在 `gateway-ai::provider` 新增异构 Provider，复用 HTTP Upstream。
- [ ] 实现 per-stream SSE/Provider 状态、错误事件和 usage 归一化。
- [ ] 增加限流、熔断、重试预算和显式兼容模型 fallback。
- [ ] 明确 SharedServices 与配置 generation 的状态继承、drain 和回收。
- [ ] 使用 Cargo feature/role 构建并部署 traffic 与 ai 两种组合。

**评审门**：Provider 合约、慢客户端、断连、429/5xx、成本、状态回收和不同 feature 构建均通过。

## 5. 阶段 3：按真实需求扩展

- [ ] 新协议使用独立 crate/binary 和协议原生 trait，不穿透 HTTP Filter。
- [ ] 出现第二个真实消费者后，才下沉公共 trait 或拆分新 crate。
- [ ] Nacos 作为独立后续任务接入，复用 `ConfigSource`，不修改 server 发布管线。
- [ ] 第三方动态插件若成为需求，独立评估 WASM/进程隔离、签名、权限、资源和背压。
- [ ] 不默认引入 Rust dylib、每 Chunk IPC、actor 式无界 channel 或通用事件总线。

## 6. 未来验证命令（当前不执行）

在获得实施授权并存在 Rust Workspace 后，至少运行：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo check -p gateway-server --no-default-features
cargo check -p gateway-server --features config-etcd
cargo tree -d
cargo tree -p gateway-core
cargo tree -p gateway-server --no-default-features
```

另需插件契约、配置 fuzz、协议集成、流式分片、故障注入、热重载、配置源乱序/断线、长连接 drain 和负载测试。`cargo tree -p gateway-core` 用于确认没有反向引入 Pingora/HTTP/AI；`cargo tree -d` 用于发现 etcd-client 与 Pingora 的重复 tonic/hyper/TLS 栈；`gateway-server --no-default-features` 用于确认未启用时没有 etcd 客户端依赖。Nacos feature 的构建检查在后续实现 crate 后加入。

## 7. 当前分析任务检查

- [x] 不编写产品代码。
- [x] 记录协议边界、风险和阶段计划。
- [x] 形成 Rust 原生 Cargo Workspace、模块和 trait 静态组合架构。
- [x] 移除 Java 式通用插件容器与过度生命周期抽象。
- [x] 增加 ConfigSource trait、local/etcd/Nacos 边界、实时下发管线和故障规则。
- [x] 明确第一期只接 etcd，Nacos 推迟，并把 etcd revision/compaction/watch 纳入阶段 1。
- [ ] 用户评审 Rust Workspace、etcd 优先方案后，再收敛最终规划；本任务不自动进入实施。
