# 统一 API 与 AI 多模态网关技术设计（分析稿）

> 状态：架构评审草案，不代表实施授权。本任务不编写产品代码。
>
> 核心结论：保留“一套代码库、声明式配置、按角色部署”的方向，但将“Pingora 统一全部协议的数据面微内核”修正为“统一控制面与策略内核 + 多个协议数据面”。Pingora 首先承担 HTTP 家族与 AI HTTP 流量，不能据此推导其天然支持 MQTT、QUIC 或 WebRTC 全栈能力。
>
> 详细协议矩阵、官方资料与 PoC 清单见 `research/protocol-and-runtime-boundaries.md`。研究核对的 Pingora 官方 `main` 快照为 `1adcb3815b70d6594379db044b5aa67f9bc1af76`，只用于确认架构方向；所有 API 和行为仍须按未来锁定版本重新验证。

## 1. 原方案评审结论

| 原主张 | 评审 | 修正方向 |
|---|---|---|
| Everything is a Stream | 作为 HTTP/TCP 抽象有帮助，作为全协议统一模型不成立 | 同时建模 Connection、Stream、Message、Request、Datagram；按协议选择能力 |
| Pingora 微内核统一所有协议 | 过度推断框架边界 | Pingora 用于 HTTP 数据面；MQTT、QUIC、WebRTC 等由独立引擎承担 |
| 统一上下文 + Option AI 上下文 | 方向可取，但当前模型混淆协议和业务，且内存表述不准确 | 公共元数据 + 协议专属上下文 + 可选 AI 扩展；AI 大对象可 boxed 或放入协议 enum |
| 所有业务都是同一种 GatewayFilter | 会把核心重新绑定到 Pingora HTTP Session | 通用策略与协议专属 hooks 分离，路由加载时编译成类型化管线 |
| 前 2~4KB + simd-json 提取 model/stream | 不具备通用正确性 | JSON 字段顺序任意；采用带界限的增量扫描、显式元数据或有界缓冲/落盘重放 |
| Arc + 原子指针交换实现热重载 | 思路正确，裸指针交换不等于安全实现 | 不可变一致快照 + 安全 Arc 生命周期/成熟快照容器 + 校验发布 + 回滚 |
| 在 upstream_response_filter 中解析 SSE body | 混淆 headers 与 body 阶段 | 按锁定 Pingora 版本使用正确的 body hook，并维护每流增量状态 |
| 触发 Hyper Flush | 与原生的 Pingora 数据路径假设不一致 | 不预设 Hyper；直接按 Pingora 的写回、背压与刷新语义验证 |
| 微秒级 TTFT | 无法作为端到端保证 | 只把解析/适配 CPU 耗时设为微秒级候选目标；端到端 TTFT 受 provider 与网络支配 |
| upstream_fail 无缝 fallback | 仅在有限阶段安全 | 响应提交前、可重放、幂等/计费允许时才重试；流已开始后不能透明换源 |
| 数据面完全无状态 | 不准确 | 不保存必须本地持久化的业务状态，但连接、解析器、限流、熔断和负载均衡均有运行态 |

## 2. 关键概念重新分层

原 `RouteType` 将互不正交的维度混在一起：`LlmInference` 是业务流量类别，`Grpc`、`Mqtt` 是应用协议，`L4Proxy` 是代理层级。AI 请求也可能通过 gRPC 传输，因此这些值不应是互斥枚举。

建议至少区分：

1. **传输层**：TCP、TLS over TCP、QUIC/UDP、WebRTC 媒体/数据通道。
2. **应用协议**：HTTP/1.1、HTTP/2、HTTP/3、gRPC、WebSocket、MQTT、SSE。
3. **代理模式**：L4 透明转发、L7 终止代理、消息感知代理、协议转换。
4. **业务类别**：普通 API、LLM inference、AI embeddings、AI audio/vision 等。
5. **部署角色**：traffic-gateway、ai-gateway、l4-gateway、mqtt-gateway、quic-gateway、webrtc-media。

SSE 是 HTTP 响应的事件编码，WebSocket 是 HTTP Upgrade 后的双向帧协议，gRPC 是基于 HTTP/2 的消息语义；三者不应被当成与 TCP/QUIC 同层的协议。

## 3. 推荐总体架构

```text
                声明式文件 / 控制面
                         |
                 校验、编译、签名、版本化
                         |
                 一致性配置快照 + 回滚
                         |
        +----------------+-------------------+
        |                                    |
  统一策略与运维内核                    统一观测与审计
  路由/鉴权/限额/超时/熔断             指标/追踪/日志/用量
        |                                    |
+-------+---------+-----------+---------+----+--------+
|                 |           |         |             |
| Pingora HTTP    | TCP/L4    | MQTT    | QUIC/H3     | WebRTC
| 数据面          | 数据面    | 数据面  | 数据面      | 独立媒体面
|                 |           |         |             |
| HTTP/H2/SSE     | 字节转发  | 会话/   | H3/流/      | ICE/DTLS/
| WS/gRPC 透明    |           | 消息代理| 数据报      | SRTP/SCTP
| AI 请求与流适配 |           |         |             |
+----------------+-----------+---------+-------------+
                         |
              上游连接与Provider连接器
        内部服务 / AI Provider / Broker / 媒体节点
```

“统一”发生在配置、策略、路由语义、provider 注册、审计和运维接口，不是强迫所有流量经过同一个 `pingora::proxy::Session`。

### 3.1 微内核的真实边界

适合放入核心：

- 配置模型、版本和依赖校验。
- 不可变快照及发布/回滚机制。
- 通用身份、租户、deadline、预算和 trace 元数据。
- 路由编译与策略定义。
- 上游抽象、错误分类、指标与审计接口。

不应放入与协议无关的核心：

- `pingora::proxy::Session`。
- HTTP Header、SSE event、gRPC message、MQTT packet 等具体类型。
- 假设所有协议都有“请求头—请求体—响应体”的单一流程。
- 依赖单一全局连接池或全局限流状态的隐式设计。

### 3.2 部署隔离

一套 Workspace 可以生成多个二进制，或由同一二进制按显式 role 启动。生产推荐按负载拆分：

- **流量网关集群**：优化高 RPS、短请求、TLS 终止、WAF 和普通鉴权。
- **AI 网关集群**：优化长连接、大 body、SSE、provider 限流、取消传播、token/cost 观测。
- **L4/MQTT 集群**：优化大量长连接、会话状态和 Keepalive。
- **QUIC/WebRTC 集群**：独立 UDP 容量、内核网络调优、丢包与媒体/数据报运维。

编译 feature 只用于裁剪依赖，不应替代运行时配置校验和安全边界。

## 4. 上下文与策略管线

### 4.1 上下文模型

公共上下文只保留跨协议真正通用的信息：连接/trace 标识、对端信息、配置快照引用、开始时间、取消信号和 deadline。以下信息应按需添加：

- HTTP：method、authority、path、headers、request/response 状态。
- AI：逻辑模型、物理模型、provider、streaming、usage、重试/计费元数据。
- L4：连接标识、目标、字节统计。
- MQTT：client id、session、topic 权限和 Keepalive。
- QUIC：connection、stream/datagram 状态。
- WebRTC：ICE/DTLS/SRTP/SCTP 会话与媒体关联。

注意：`Option<T>` 通常在包含它的结构体中保留 `T` 的 inline 空间；`None` 不等于零内存开销。若基础上下文大小是硬要求，应采用协议 enum、受控的堆扩展或 `Option<Box<T>>`，并通过基准选择，而非只看类型写法。

`client_id` 应在可信认证后生成；外部传入的 request/trace id 必须按信任边界清洗，不能直接作为可信身份。

### 4.2 不采用单一万能 Filter trait

建议把策略分成三类：

1. **协议无关决策**：tenant/authz、配额、静态路由约束、熔断决策。输入是规范化元数据，不接触 Pingora Session。
2. **协议专属生命周期 hooks**：HTTP headers/body、WebSocket frames、gRPC streams、MQTT packets 等各自定义。
3. **AI 领域适配器**：请求映射、provider 选择、SSE event 转换、usage 归一化，不作为通用字符串过滤器堆叠。

HTTP 管线至少需要区分：路由前、请求头、请求 body、上游选择、上游请求、响应头、响应 body、流结束、取消/错误。仅有 `request_filter` 和 `response_chunk_filter` 无法表达缓存短路由、失败切换、半关闭、取消和流结束。

过滤器链应在配置加载时完成注册表解析、顺序校验和编译；热路径只读取已编译快照，避免每次请求按字符串查插件或组装临时 Vec。协议转换不要伪装成“通用鉴权过滤器”。

## 5. AI 大 Body：正确性优先于局部解析

### 5.1 为什么前 4KB 方案不可靠

- JSON 对象字段没有顺序保证，`messages` 可以合法地位于 `model` 前。
- `model` 可能不存在、出现在深层嵌套中、重复出现，或被转义/空白影响。
- body 可能采用 chunked/H2 DATA 任意分块，也可能压缩；网络 Chunk 与 JSON 边界无关。
- 只读取前缀会留下尚未转发的数据；若后续更换上游，还涉及重放和连接背压。
- `simd-json` ondemand 的“按需访问”不能直接等同于“对不完整网络前缀进行流式解析”。具体 buffer/完整性要求应按锁定版本验证，不能把它当成任意前缀解析保证。

### 5.2 推荐的分级策略

**优先级 A：路由不依赖 body。** 通过 vhost、path、明确的路由配置或可信 header 决定 provider；body 只做必要的受控处理。这是最低延迟方案。

**优先级 B：产品协议强制提供路由元数据。** 对内部 API 可要求显式 model/provider hint，但仍需鉴权与合法性校验。公共 OpenAI 兼容接口不能假设所有客户端都会提供额外 header。

**优先级 C：有界增量扫描。** 使用能跨 Chunk 保存状态的 JSON tokenizer，只跟踪顶层字段，同时原样保留已读字节。若 provider 选择依赖 model，则在字段出现前不能向最终上游提交请求。超过扫描/时间/内存界限时按明确规则拒绝或走默认路由，不能悄悄猜测。

**优先级 D：有界缓冲或磁盘 spool 后重放。** 当字段可能位于任意位置且必须先选上游时，只能等待足够信息，最坏情况下需要读完整 body。磁盘 spool 可降低内存，但会增加 TTFT、I/O 和敏感数据治理成本。

任何方案都要处理：原始前缀只重放一次、后续 Chunk 不丢失、客户端取消、最大 body、解压炸弹、非法/重复字段、上游重试是否可重放。跨 provider 请求语义转换与纯透传不同；tools、vision、audio 等复杂 payload 可能需要完整语义解析，不能继续宣称全链路零拷贝。

“减少 90% 内存/CPU”只能作为待基准验证的假设，不能作为设计结论。

## 6. SSE 与流式转换

### 6.1 解析器要求

每个上游响应流持有独立解析状态，至少处理：

- 任意位置拆分的行、CRLF、多行 `data:`、注释/心跳和空行事件边界。
- 跨 Chunk 的 UTF-8 字符和 JSON 文档。
- `event:`、`id:`、`retry:`、`data: [DONE]`、provider error event。
- 上行提前结束、下游断开、idle timeout 与半关闭。
- 未知事件保留/丢弃策略和输出大小上限。

可用 memchr 加速换行查找，但零拷贝只适用于当前 buffer 内完整数据；跨界 remainder 通常需要少量拷贝。转换后的 OpenAI event 也需要分配和序列化，不能把整个适配过程描述成绝对零分配。

### 6.2 Pingora 集成边界

响应头回调与响应体回调必须分开。原方案把 SSE body 解析放在 `upstream_response_filter` 的阶段假设需要修正；具体 hook 名称、能否直接控制刷新，以及压缩/缓冲交互都按锁定版本的源码和 PoC 验证。

不预设通过 Hyper Flush 写 socket。网关应表达“事件可转发时尽快写回”，同时服从 Pingora 和客户端的背压。无界地产出并立即 flush 可能放大慢客户端内存与系统调用开销。

### 6.3 性能指标

分别观测：

- provider 到网关首字节/首事件时间。
- 网关解析与适配 CPU 时间。
- 网关排队和写回等待时间。
- 客户端可见 TTFT、tokens/s、流持续时间和取消率。

只有网关内部 CPU 阶段可以考虑微秒级优化目标，端到端 TTFT 不应承诺微秒级。

## 7. 配置快照与热重载

推荐发布流程：

1. 接收文件或控制面版本，执行语法解析。
2. 校验 route/upstream/filter/provider/secret 引用、冲突、优先级和资源限制。
3. 在后台编译完整不可变快照。
4. 可选执行小流量语义检查或 dry-run。
5. 原子发布新 generation；每个请求/流固定引用一个快照。
6. 在途请求自然结束并释放旧版本；保留 last-known-good 和版本指标。
7. 失败只报告状态，不影响当前快照；支持回滚到已知版本。

读路径目标是“无 RwLock 争用且生命周期安全”，而不是手工 Atomic Pointer Swap 后声称严格 lock-free。建议使用经过验证的 Arc 快照机制；其实现和性能按依赖版本验证。

路由快照不应只有 Trie，还应编译 vhost、method、path 模板、headers、协议、优先级和冲突规则。TLS 证书、连接池、DNS 和已建立的长连接有各自 draining/reload 生命周期，不能因换路由指针就自动完成热切换。

数据面可以不暴露业务配置增删改 API，但至少需要只读版本、reload 状态、健康/就绪和审计信息。

## 8. Outbound、重试与 AI fallback

### 8.1 连接复用

- HTTP/H2/H2c 与外部 HTTPS 可以有各自的 peer 池，但池键必须覆盖 authority、SNI、TLS/客户端证书、HTTP 版本、代理和安全配置等维度。
- 不假设 Pingora 的池是跨所有 worker 的进程全局池；实际作用域按版本与部署拓扑验证。
- MQTT、QUIC、WebRTC 不复用 HTTP peer pool。
- `least_latency` 需要定义冷启动、样本窗口、异常值、TTFT 与整流时长的区别；AI 应按 provider/model/region 统计，而非只看地址。

### 8.2 重试安全矩阵

| 状态 | 默认策略 |
|---|---|
| 尚未成功连接/发送请求 | 可在重试预算内换端点 |
| 请求部分或全部发送，响应未开始 | 只有可重放且业务允许时重试；AI 需防重复生成/计费 |
| 收到错误响应头，尚未提交下游 | 可按显式策略映射、重试或 fallback |
| 已向下游发送响应头/body | 不自动换上游；只能按协议终止或发送可表达的错误事件 |
| SSE 中途断流 | 默认终止并记录；续传需 provider 与业务共同支持，不等于普通 retry |

引入 per-tenant/provider retry budget、指数退避与 jitter、熔断、429/5xx 分类、最大总 deadline。模型 fallback 必须显式声明能力兼容性（上下文长度、tools、vision、JSON mode 等）及成本上限，不能把不同模型当成无差别 IP 备份。

## 9. 协议支持矩阵与边界

| 协议 | 建议支持定义 | 数据面/备注 | 推荐阶段 |
|---|---|---|---|
| HTTP/HTTPS、HTTP/1.1、HTTP/2 | L7 终止与反向代理 | Pingora 核心场景；TLS/ALPN/版本能力按锁定版本验证 | 第一阶段 |
| SSE | 流式响应转发；AI 场景事件转换 | HTTP body 之上的状态机，不是独立传输 | 第一阶段透传，第二阶段转换 |
| WebSocket | HTTP Upgrade 与双向帧/字节转发 | Upgrade 后普通 HTTP body filter 不等于消息感知；消息拦截需另行终止协议 | 第一阶段透明代理 |
| gRPC | 首先支持 HTTP/2 透明代理 | 可转发不等于支持 message 级鉴权、反射或转码；H2 flow control 必须验证 | 第一阶段透明，后续消息感知 |
| TCP | L4 连接代理 | 独立连接上下文与字节统计；不能使用 HTTP 路由/filter | 第三阶段 |
| MQTT | TCP/TLS 上的连接代理或消息感知代理 | 长连接、Keepalive、会话/topic 状态；不等于 MQTT broker | 第三阶段，先定义代理层级 |
| QUIC（待确认 quick） | 需区分 QUIC 代理、HTTP/3 终止、WebTransport | UDP、多流、连接迁移、datagram；需要独立协议引擎与版本验证 | 独立后续阶段 |
| WebRTC | 需区分信令、TURN/STUN、DataChannel、SFU | ICE/DTLS-SRTP/SCTP、NAT 与媒体面；不是普通 API 反向代理 | 独立后续阶段 |

WebSocket 与 gRPC 的透明代理可在第一阶段验证，但“消息内容过滤/转换”不属于该承诺。MQTT 消息级路由需要解析 packet 和会话语义；WebRTC 若目标是 SFU，应单独选型，不应为了统一而自行扩展 HTTP 网关承担媒体服务器。

## 10. 声明式配置模型的修正

原 YAML 中有三个需要立即修正的地方：

- `"://openai.com:443"` 和 `"://company.com"` 不是完整合法的地址/vhost 表达；应使用明确 hostname、port、scheme 分离字段。
- `type: dynamic_llm` 混淆服务发现与 AI provider 语义；服务发现（static/DNS）、负载均衡、AI provider adapter 应分别建模。
- filters 字符串必须经注册表和顺序校验；未知插件、重复引用或不兼容协议应让配置发布失败。

配置还应覆盖：路由优先级、body/timeout 上限、TLS 校验与 SNI、重试预算、fallback 条件、流式开关、消息大小、连接 draining、观测采样。secret 只保存引用，不在路由快照或日志中明文展开。

## 11. Crate 与依赖边界建议

不建议把所有协议塞进单个 `nico-protocols`，可按依赖和发布角色拆分：

- `nico-server`：启动、角色选择、信号与生命周期。
- `nico-core`：配置、快照、公共元数据、策略和错误模型；不依赖 Pingora HTTP Session。
- `nico-http`：Pingora 接入和 HTTP 专属 hooks。
- `nico-llm`：provider/model 领域、AI 请求与 SSE 适配。
- `nico-policy` 或少量插件 crate：鉴权、限流、安全策略。
- 后续独立的 `nico-l4`、`nico-mqtt`、`nico-quic` 等。

WASM 插件不建议进入第一阶段。流式 ABI、异步取消、内存/fuel 限制、宿主能力和版本兼容会显著放大核心攻坚范围，应在同步 Rust 管线稳定后独立评估。

## 12. 安全、观测与容量约束

- 认证/限额/body 上限尽可能早于昂贵 JSON 或协议转换。
- provider 凭证按租户/路由注入，禁止客户端通过任意 URL 指定上游形成 SSRF。
- 默认不记录 prompt、response 内容、Authorization 和 provider key；审计优先记录元数据。
- 统一 trace，但区分 request、connection、stream 和 provider call；retry 不产生重复业务审计。
- usage 必须标明来自 provider、协议最终事件还是本地估算，不能混为精确账单。
- 慢客户端、最大流时长、idle timeout、并发流、每租户 egress 与取消传播都是 AI 数据面必需限制。
- 数据面可替换不等于完全无状态：本地连接、限流器、熔断器、负载指标和 MQTT 会话必须明确其持久性与故障恢复方式。

## 13. 分阶段落地与退出条件

### 阶段 0：能力锁定与风险 PoC

- 锁定 Rust/Pingora/TLS 依赖版本，验证 H1/H2、SSE、WebSocket、gRPC 透明代理的实际回调、背压和连接池语义。
- 验证任意分块的大 body、model 位于 4KB 后、SSE 每字节拆分、慢客户端和配置热重载。
- 明确 WebRTC/QUIC/MQTT 的支持等级，不把未验证能力写入对外兼容承诺。

### 阶段 1：HTTP + AI 透传 MVP

- 声明式配置、不可变快照、HTTP 路由、认证/限额、基础观测。
- HTTP/HTTPS、H1/H2、SSE 透传；WebSocket/gRPC 透明代理按 PoC 结果进入。
- OpenAI 兼容 provider 的受控转发、凭证注入、超时、取消、响应前有限重试。
- 退出条件：正确性/资源上限/热重载/慢客户端测试通过，无跨 Chunk 丢字节，无响应提交后的隐式重试。

### 阶段 2：AI 语义适配与韧性

- 至少一个异构 provider（如 Anthropic）的请求和 SSE event 适配。
- 增量状态机、usage 归一化、provider/model 维度限流与熔断、兼容模型 fallback。
- AI 与流量集群独立扩缩容，建立 TTFT、tokens/s、取消和成本观测。
- 退出条件：provider 合约测试、任意分片/错误事件/断连测试、重试与计费测试、过载与回滚演练通过。

### 阶段 3：L4、MQTT 与 gRPC 深化

- TCP 使用独立 L4 数据面；MQTT 先实现明确层级的代理，再决定是否需要 topic/message 能力。
- gRPC 仅在业务需要时加入 message 级解析、方法级策略或转码。
- 退出条件：长连接、半关闭、Keepalive、会话恢复、H2 flow control 和升级不中断策略通过验证。

### 阶段 4：QUIC/HTTP3 与 WebRTC 独立演进

- 根据确认后的产品目标选择 QUIC/H3 引擎或 WebRTC/TURN/SFU 组件。
- 独立 UDP/媒体容量模型和故障域，复用控制面与策略，不强塞入 HTTP filter chain。
- 退出条件按协议单独定义，包括迁移/丢包/NAT/媒体或 datagram 测试。

## 14. 必测场景

1. `model`/`stream` 位于 4KB 之后、位于 `messages` 之后、字段重复、字段缺失、非法 JSON。
2. chunked/H2 任意分块、压缩 body、超大 multimodal payload、客户端中途取消。
3. SSE 在每个可能字节位置拆包，含 CRLF、多行 data、心跳、错误事件和 `[DONE]`。
4. 慢客户端背压、provider 中断、下游断连、idle/total timeout、内存是否有界。
5. 响应提交前后分别注入连接失败、429、5xx、reset，验证无非法重试和重复计费。
6. 高并发热重载、错误配置、回滚、在途流仍用旧快照、worker 间版本可观测。
7. WebSocket Upgrade、gRPC 多路流/flow control、HTTPS SNI/证书校验、H2c 与 HTTPS 池隔离。
8. 后续协议分别验证 MQTT 会话、QUIC 多流/迁移、WebRTC ICE/NAT/DTLS，而非用 HTTP 测试替代。

## 15. 当前决策与待确认项

已确定的架构建议：HTTP/AI 先行；统一控制面而非统一所有数据面；通用策略与协议 hooks 分离；正确性和资源界限优先于局部解析、零拷贝与微秒级宣传指标。

仍需用户决定：WebRTC 的支持层级、`quick` 的真实含义、第一期 provider 和 SLA。上述待确认项不妨碍 HTTP/AI 架构分析，但会改变后续独立数据面的选型与排期。
