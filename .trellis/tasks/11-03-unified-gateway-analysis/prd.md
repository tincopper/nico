# 统一 API 与 AI 多模态网关方案分析

## Goal

形成一套可评审、可分阶段验证的 Rust 原生网关架构：以 Cargo Workspace、少量 crate、模块和领域 trait 构成微内核 + 插件；通过静态组合、类型化配置和预编译路由链扩展能力。Pingora 仅用于 HTTP 数据面，API 网关与 AI 网关由不同模块和 Cargo feature 组合，并支持按角色独立部署。

本任务只交付分析、设计和技术验证计划，不编写产品代码。WebRTC 经用户确认暂不纳入当前架构范围。

## Background and Confirmed Scope

- 当前明确方向是“微内核 + 插件、声明式多形态部署、执行图/过滤器链拓扑驱动”。
- 目标同时覆盖普通 API 流量与大模型推理流量，并支持独立集群部署，避免两类负载共享故障域。
- 用户最初列出的协议为 `quick`、MQTT、gRPC、WebSocket、SSE、HTTP/HTTPS、HTTP/2、TCP、WebRTC；WebRTC 现已明确暂缓。本方案暂将 `quick` 视为 QUIC 的潜在笔误，但不代替用户确认。
- 当前仓库没有待改造的产品代码或既有网关实现，因此本任务不是代码改造，而是架构评审与落地设计。
- 第一期实时配置中心确定为 etcd；Nacos 保留 `ConfigSource` 实现位置，但推迟到后续里程碑。
- Pingora 具体版本、特性和回调语义尚未锁定；所有依赖具体版本能力的结论必须在实施前通过官方源码、文档和 PoC 验证。

## Requirements

### R1：微内核统一机制，插件承载协议与业务

- 微内核只提供稳定机制：插件注册与依赖、配置编译与快照、执行图、生命周期、取消/deadline、错误与观测。
- Pingora、HTTP、AI Provider、JWT、限流、MQTT 等均不得成为协议无关内核的强制依赖。
- 明确连接、流、消息、请求/响应、数据报等不同语义，不能以单一双向字节流替代全部协议模型。
- 区分透明转发、协议终止、消息感知代理和协议转换四种“支持”等级。
- 统一控制面、配置快照、策略决策、指标与审计；允许 HTTP、MQTT、QUIC 等使用独立数据面或进程角色。

### R2：明确 HTTP 与 AI 网关的 MVP 边界

- HTTP 数据面优先覆盖 HTTP/HTTPS、HTTP/1.1、HTTP/2、SSE、WebSocket，以及 gRPC 的 HTTP/2 透明代理能力。
- AI 数据面优先覆盖 OpenAI 兼容 API 和至少一种异构 provider 的请求/响应适配，并支持 SSE 流式转发。
- gRPC 消息感知、MQTT、TCP、QUIC/HTTP3 是否进入后续阶段，应由各自独立验收标准驱动；WebRTC 当前暂缓。

### R3：大请求体处理必须有正确性和资源上限

- 不假设 `model`、`stream` 一定位于请求前 2~4KB。
- 不把 `simd-json` ondemand 等同于网络流式 JSON 解析器。
- 路由所需元数据无法可靠获得时，必须有明确降级策略。
- 转发、重试和转换期间必须保留原始字节，处理任意 body 分块，并设置 body、解压、解析时间和内存上限。

### R4：流式适配必须支持跨 Chunk 状态

- SSE 解析必须处理跨网络 Chunk 的事件、行、UTF-8 和 JSON 边界。
- 转发路径应避免不必要的整事件或整响应缓冲，并明确背压、慢客户端、取消和断连语义。
- 性能目标必须区分网关内部处理耗时与端到端 TTFT，不承诺不可控的端到端微秒级指标。

### R5：配置热重载必须安全且可回滚

- 使用一致的不可变配置快照，读路径避免 `RwLock` 争用。
- 新配置必须经过解析、引用校验、语义校验和构建成功后才能发布。
- 发布失败保留 last-known-good；在途请求继续使用旧快照；支持快速回滚。
- 不用手工裸指针交换替代安全的生命周期管理。

### R6：故障切换不得破坏请求语义

- 区分连接失败、响应前失败、响应已提交、流式中途失败等阶段。
- 只有请求可重放、操作允许重试且未向客户端提交响应时，才允许自动重试或 fallback。
- AI fallback 必须考虑模型能力兼容、重复计费、幂等、限流、重试预算和熔断。
- 不假设所有 Pingora 连接都在整个进程或所有 worker 间共享同一个全局池。

### R7：部署形态与资源隔离

- 一套 Workspace 可产出不同进程角色或部署单元，但不要求所有协议运行在同一个进程。
- 流量网关与 AI 网关应能独立扩缩容、升级、限流和故障隔离。
- 每类数据面定义自己的容量指标、连接生命周期和资源限制。

### R8：安全与可观测性

- 覆盖认证鉴权、租户隔离、限流、请求大小限制、超时、TLS/SNI、secret 管理和审计。
- 统一 request/connection/trace 标识的关联方式，但保留各协议原生语义。
- AI 可观测性区分网关指标、provider 返回的 token usage 和本地估算，避免把估算当精确账单。

### R9：Rust 原生插件契约与组合

- 插件首先是实现领域 trait 的 Rust crate 或 module，不引入 Java 式 Plugin Container、通用 SPI 层或 Manifest 生命周期框架。
- trait 定义在领域拥有者 crate 中，例如 `HttpFilter` 属于 HTTP crate、`Provider` 属于 AI crate；不集中放入万能 `nico-spi`。
- 优先 module，只有需要独立编译、依赖隔离或复用时才拆 crate；Provider 初期作为 AI crate 内部模块。
- 启动时由 `nico-server` 显式注册具体实现；配置加载时可按类型名调用 builder，热路径只持有已构建的 trait object 或 enum。
- 配置使用 `serde` 反序列化为各插件的类型化 Config；路由发布前编译为不可变调用链，热路径不按字符串发现插件。
- 闭合集合并需要极限性能时优先 enum/泛型；开放扩展点使用少量 `Arc<dyn Trait>`，不追求全系统静态分发或全系统动态分发。
- 流式适配器拥有 per-stream Rust 状态对象，不使用全局 Plugin Instance 保存请求状态。

### R10：配置来源 trait 与实时下发

- 通过 `ConfigSource` trait 支持 local 文件与 etcd；Nacos 作为后续实现，接口保持兼容。第一期只交付 local + etcd。
- 每个进程选择一个权威来源；第一期 etcd 是生产权威来源，local 同时承担文件模式和 last-known-good 缓存。
- 配置源只负责加载/订阅完整文档及提供 source-scoped revision，不解析路由、不直接修改运行快照。
- 配置源 trait 放在独立 `nico-config` crate；etcd 实现为可选 crate + `config-etcd` feature，Nacos crate 推迟创建。
- 启动所需的来源类型、地址、认证引用、缓存路径和资源上限只来自本地 bootstrap，不能来自配置中心自身。
- server 统一执行去抖、single-flight、revision 排序、schema/引用校验、编译和 ArcSwap 发布。
- watch 必须可取消、有界、可重连；断线、compaction、乱序、重复和慢消费者不得导致错误快照发布。
- 新配置编译失败保留 last-known-good；配置中心不可用期间继续服务并标记 degraded。
- 默认不支持增量 patch；多 key 场景必须全量重读并保持完整文档一致性。
- 配置中心视为高权限不可信输入：TLS/认证、大小/深度/数量限制、严格 schema、secret 仅存引用。

### R11：Cargo 交付、故障与可选动态扩展

- 第一阶段默认静态链接和显式组合，Cargo feature 用于裁剪 AI、MQTT、WASM 等可选能力，运行时配置用于路由和策略。
- 配置热重载使用简单 reload 流程与 `ArcSwap` 快照，不建设通用 Plugin Lifecycle Manager；二进制变化走滚动升级。
- 不把 Rust 动态库的 unstable ABI 作为默认插件机制；第三方动态插件需求出现时再独立评估 WASM 或进程隔离。
- 原生插件与宿主同一故障域；是否同进程是部署选择，高风险协议可独立 binary/process。
- 避免 Manager/Service/Factory 层层包装、全局 Service Locator、无约束 `Any`/字符串对象袋以及为每个能力机械拆 crate。

## Acceptance Criteria

- [x] 给出 Rust 微内核职责、6 个初始 crate（另含 2 个可选配置源 crate）的边界、模块划分和依赖方向。
- [x] 给出领域 trait、静态注册、serde 类型化配置、CompiledRoute 与 ArcSwap 热重载设计。
- [x] 给出 ConfigSource trait、local/etcd 实现边界，以及 Nacos 的后续扩展位和故障降级规则。
- [x] 说明配置源 revision、去抖、single-flight、多 key 一致性、bootstrap 与 LKG 缓存设计。
- [x] 明确第一期只接 etcd，Nacos 推迟；etcd 的 revision/compaction/watch 重连/认证 TLS 纳入第一阶段验收。
- [x] 核查 etcd Watch 包含式 revision、compaction、fragment、默认 value 上限，以及 etcd-client 候选版本的 API/依赖风险。
- [x] 给出原生静态组合、WASM、进程隔离和 Rust 动态库的选型与故障边界。
- [x] 用普通 API 与 AI 两条链路证明同一 Workspace 可通过不同插件组合运行。
- [x] 消除通用 SPI 容器、Manifest 框架、Lifecycle Manager 和 Provider 微 crate 等 Java 式过度设计。
- [x] 给出原方案的逐项评审，明确指出成立条件、错误假设和修正方向。
- [x] 给出协议支持矩阵，至少包含传输、代理层级、Pingora 适配程度、状态模型、建议阶段。
- [x] 给出修正后的分层架构、上下文边界、过滤器/策略边界和配置快照模型。
- [x] 给出大 body 路由、原始流转发、SSE 增量解析和背压的正确性约束。
- [x] 给出连接复用、重试、fallback、熔断和重复计费风险的约束。
- [x] 给出不少于三个阶段的实施路线，每阶段有独立 PoC 与可验证退出条件。
- [x] 标记所有需要按锁定 Pingora 版本验证的能力，不将推测写成已证实事实。
- [x] 全程不创建或修改 Rust/Cargo 产品代码。

## Out of Scope

- 本任务不实现 Workspace、crate、过滤器、协议适配器或配置中心客户端。
- 不实现配置中心管理后台、多来源合并、跨集群事务发布或配置签名基础设施。
- WebRTC 当前整体暂缓，不设计 SFU、媒体转发、ICE、STUN/TURN 或 DataChannel 能力。
- 不在未定义 QUIC 目标的情况下承诺 HTTP/3、WebTransport 或 QUIC 四层代理。
- 不承诺具体吞吐、延迟、节省 90% 内存/CPU 等数值，除非后续有可复现基准。
- 不一蹴而就地支持全部 AI 厂商、全部 gRPC 反射/消息转换或完整 MQTT broker 能力。

## Open Product Decisions

1. 插件是否必须支持第三方在不重新编译/重启主程序的情况下交付和加载尚未确定。当前建议第一阶段使用受信任的静态链接 Rust 插件；若该要求成立，再选择 WASM 或进程插件，而不是默认使用 Rust dylib。
2. `quick` 是否确指 QUIC，以及目标是否是 HTTP/3 入口、QUIC 连接代理或 WebTransport，尚待确认。
3. 第一期优先的业务 SLA、最大请求体、AI provider 列表和部署环境尚未给出；当前设计只给建议默认值和验证方法，不替用户作产品承诺。
4. etcd 已确认作为第一期配置中心，Nacos 推迟。仍需确认 etcd 中采用“一个集群一份完整文档”还是多 key；当前推荐单 key 完整文档，以降低多 key 一致性风险。
5. 启动时配置中心不可用是否允许用 last-known-good 缓存启动；当前建议默认不允许，只有显式配置才降级为 stale 启动。
