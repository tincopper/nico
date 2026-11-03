# 协议与运行时边界技术分析

日期：2026-11-03  
范围：Rust + Pingora 统一 API 网关与 AI 网关；仅做技术分析，不涉及产品代码修改。

> 后续变更：用户已确认 WebRTC 暂缓，不纳入当前架构；配置来源 local/etcd/Nacos 的 trait 设计见 `design.md` 第 8 节及 `config-source-options.md`。

## 0. 核心结论

1. **合理目标是“统一控制面、分协议数据面”，不是让所有协议进入同一个 Pingora HTTP `Session`。** 配置、策略、服务发现、审计和指标可以统一；HTTP、TCP、QUIC、MQTT、WebRTC 的连接与状态机不能靠一个字节流接口统一。
2. Pingora 的 `pingora-proxy` 适合作为 **HTTP/1、HTTP/2 数据面**。SSE、gRPC、WebSocket 可以复用部分 HTTP 代理能力，但“能代理 HTTP 流量”不等于“理解 gRPC 消息、WebSocket 帧或 SSE 事件”。
3. **QUIC 不等于 HTTP/3，MQTT 不等于长连接 TCP，WebRTC 也不等于 UDP 转发。** 三者都需要独立的协议状态与验收标准；WebRTC 还需要独立的媒体/NAT 穿透数据面。
4. “只读前 2～4KB 提取 `model`/`stream`”只能作为受约束 API 的可选快速路径，不能作为通用 OpenAI 兼容接口的正确性依据。
5. `Arc` 本身不是可原子替换的配置槽；热重载还必须处理配置一致性、旧连接 draining、连接池及长期会话。
6. “Hyper Flush 保证微秒级 TTFT”和“上游失败后无缝 fallback”均不成立。前者混淆本地写缓冲与端到端首 token 延迟，后者忽略非幂等请求、重复计费和已经提交的下游响应。
7. 建议 MVP 首先明确支持 **HTTP/HTTPS、HTTP/2、SSE、gRPC/WebSocket 的限定代理模式**；TCP 使用独立 L4 数据面。QUIC/HTTP3、MQTT 消息感知、WebRTC 媒体能力应在独立 PoC 通过后再承诺。

## 1. 证据边界与“支持”的定义

### 1.1 仓库事实与版本边界

- 当前仓库没有 `Cargo.toml` 或 `Cargo.lock`，没有可核对的 Pingora、Hyper、simd-json 或 arc-swap 依赖及 feature。因此，**本文不假定任何项目内已锁定的 Rust API 或框架能力**。
- 外部核对材料为 Pingora 官方 `main` 分支，快照提交为 `1adcb3815b70d6594379db044b5aa67f9bc1af76`。其 README 公开列出 HTTP/1、HTTP/2 端到端代理、gRPC 和 WebSocket 代理、TLS、可定制负载均衡/failover、graceful reload；`pingora-proxy` 文档描述了 HTTP 请求阶段。该快照用于理解架构方向，**不证明项目未来锁定版本具有相同 API**。
- 下文凡涉及具体类型路径、回调签名、feature、重试行为、WebSocket 升级方式、body flush 或 graceful reload 实现，统一适用：**实施前需按锁定版本验证**。
- “quick”按 **QUIC 的潜在笔误**分析，仍标记为**待用户确认**。如果实际指其他名为 Quick 的协议，QUIC 相关结论不能直接套用。

### 1.2 三种“支持”必须分别验收

| 支持层级 | 实际含义 | 能做/不能做什么 |
|---|---|---|
| **透明转发（T）** | 转发 TCP 字节流或 UDP 数据报；可终止/建立新的传输连接，但不解释应用消息 | 可做 L4 路由、连接控制、字节级统计；TLS passthrough 下不能做 HTTP/topic/model 策略 |
| **协议终止（P）** | 网关是下游协议端点，并建立独立上游连接 | 必须处理握手、关闭、错误、流控、定时器和状态映射；终止 HTTP/TLS 并不自动意味着理解其承载的消息 |
| **消息感知（M）** | 解析并操作应用层语义 | 例如 JSON 字段、gRPC message/trailer、MQTT topic/QoS/session、WebSocket frame、SSE event、WebRTC RTP/RTCP/SSRC；需要专门 schema、状态机及安全约束 |

因此，“支持 MQTT”可能仅表示 TCP 透传，也可能表示 MQTT broker proxy；“支持 WebRTC”可能仅表示 HTTP 信令、TURN relay，或完整 SFU。三者成本差异巨大，不能用同一个复选框表示。

## 2. 协议不是平级列表：先分层再选数据面

```text
TCP ── TLS ── HTTP/1.1 或 HTTP/2 ── AI JSON / SSE / gRPC / WebSocket
UDP ── QUIC ── HTTP/3 ────────────── HTTP 语义及可能的 AI/SSE 等承载
TCP/TLS 或 WebSocket ── MQTT ─────── topic、QoS、持久会话
UDP/TCP relay ── ICE/STUN/TURN ── DTLS ── SRTP 或 SCTP ── WebRTC 媒体/数据
```

- HTTPS 是 HTTP 的 TLS 安全承载，不是与 HTTP 平级的另一种应用协议。
- HTTP/2 是 HTTP 的一种传输映射；gRPC、SSE、WebSocket 又具有各自的会话或消息语义。
- QUIC 是通用传输，HTTP/3 只是其一种应用映射；终止 HTTP/3 不等于能代理任意 QUIC 应用。
- WebRTC 的信令通常走 HTTP/WebSocket 等，但信令代理不能替代 ICE、DTLS-SRTP、SCTP 或 TURN 数据面。

### 2.1 全协议边界矩阵

| 协议 | 传输与会话语义 | 能否自然进入 Pingora HTTP 数据面 | 三种支持边界 | 需要的独立能力/数据面 |
|---|---|---|---|---|
| **HTTP/HTTPS** | HTTP/1.x 通常为 TCP 上请求/响应，可含流式 body、upgrade；HTTPS 增加 TLS 端点与证书/ALPN | **可以，核心场景。** Pingora 的 HTTP 代理生命周期与该模型匹配；具体 TLS 后端和 API **实施前需按锁定版本验证** | T：TCP/TLS passthrough，仅 L4；P：终止 TLS/HTTP 并发起上游请求；M：基于 header/body/schema 做路由、WAF、AI 策略 | 非 HTTP TCP tunnel 需 L4 数据面；HTTP 本身不需要另造运行时 |
| **HTTP/2** | TCP/TLS 上的二进制帧；单连接多 stream，HPACK、双向流控、RST_STREAM/GOAWAY；上下游 stream 不必一一对应 | **可以，核心场景。** 但 H2 特性、上下游版本转换及超限行为 **实施前需按锁定版本验证** | T：整条 TCP 连接透传，无法按 stream 路由；P：按 HTTP stream 终止并代理；M：还需理解 gRPC 等上层消息 | 若要求保留端到端同一 H2 连接语义或实现自定义帧处理，需专门 H2 层；普通反向代理不需要 |
| **gRPC** | 通常基于 HTTP/2；支持 unary 与 client/server/bidi streaming；length-prefixed message、压缩标志、metadata、deadline/cancellation、`grpc-status` trailers | **可自然做 HTTP/gRPC 透传代理；不能据此声称消息感知。** 官方快照宣称 gRPC proxying，具体覆盖 **实施前需按锁定版本验证** | T/P：转发 HTTP/2 与 body/trailer，保持流式与取消；M：解析 service/method、消息 codec、压缩、状态和业务字段 | 消息级策略、Protobuf 校验、RPC 级重试需 gRPC codec/策略层；不要缓存整个流 |
| **WebSocket** | 常见为 HTTP/1.1 Upgrade，也有 H2 extended CONNECT；握手后是长期双工 frame/message，含分片、ping/pong、close、 masking，可能有压缩与子协议 | **可做限定代理，不是通用 HTTP 请求。** 官方快照宣称 WebSocket proxying；H1/H2 入口、upgrade 后回调可见性、压缩支持 **实施前需按锁定版本验证** | T：握手后字节/帧隧道；P：网关分别终止上下游 WebSocket 并处理 frame 状态；M：按消息/subprotocol 做检查或路由 | 消息感知、广播、presence 或 Pub/Sub 需独立长连接服务；MQTT over WebSocket 仍需 MQTT 层 |
| **SSE** | 普通 HTTP 响应，`text/event-stream`；服务端到客户端的事件流，含 `data`/`event`/`id`/`retry`、注释心跳与 `Last-Event-ID` 重连语义 | **可以，属于流式 HTTP 响应。** 具体 body 转发/flush 行为 **实施前需按锁定版本验证** | T/P：不解释事件地转发响应；M：解析、过滤、重写、回放事件；回放还需事件存储和游标 | 普通透传无需独立数据面；可靠 resume/replay 需要事件日志，不是 flush 能解决的问题 |
| **TCP** | 可靠、有序、全双工字节流；无消息边界，有 half-close、流控、拥塞控制与长期连接 | **不属于 `pingora-proxy` 的 HTTP 请求模型。** 即使复用 Pingora 的底层网络/runtime 组件，也应视为另一数据面 | T：双向 byte relay/L4 LB；P：分别终止两条 TCP 连接但仍只转发字节；M：必须先指定 TLS、MQTT 等上层协议 | 独立 accept/read/write/copy 循环、half-close、背压、空闲超时、连接配额、SNI/元数据路由（如需要） |
| **QUIC（quick，待确认）** | UDP 上加密传输；Connection ID、多路 stream、流控、丢包恢复、连接迁移，可选不可靠 datagram | **不能自然塞入 Pingora H1/H2 HTTP 数据面。** 所核对官方快照没有据此承诺通用 QUIC/H3 能力；实际版本 **实施前需按锁定版本验证** | T：UDP/QUIC 感知 relay/LB；P：终止 QUIC；M：还需终止并理解 HTTP/3 或其他 QUIC 应用协议 | 独立 UDP socket、QUIC endpoint、CID 路由、定时器/加密/迁移；若支持 H3，还需 H3+QPACK 与 H1/H2 转换层 |
| **MQTT** | 常见为 MQTT 3.1.1/5 over TCP/TLS，也可走 WebSocket；长期连接，CONNECT/CONNACK、订阅、PUBLISH、QoS 0/1/2、keepalive、Will、persistent session | **MQTT 本身不能进入 HTTP `Session`。** 只有外层 WebSocket 握手可能复用 HTTP 数据面 | T：TCP/TLS 透传，由 broker 保有全部会话；P：终止 MQTT 并桥接另一 broker 连接；M：按 ClientID/topic/message/QoS/ACL 处理 | 独立 MQTT codec 与会话状态机；持久会话、QoS、packet identifier、重连和 broker 粘滞路由，见 §3.2 |
| **WebRTC** | 不是单一协议；信令带外，媒体/数据经 ICE 候选与连通性检查，通常 UDP 上 DTLS-SRTP 或 SCTP over DTLS；TURN 可提供 UDP/TCP/TLS relay | **仅 HTTP/WebSocket 信令可复用 Pingora；媒体/数据不能。** | T：限定 UDP relay 或标准 TURN；P：DTLS/SRTP/SCTP 端点；M：SFU/媒体服务理解 RTP/RTCP、SSRC/MID、拥塞与重传等 | 独立 ICE/STUN/TURN、DTLS-SRTP/SCTP、媒体转发与高 pps UDP/TCP 数据面，见 §3.3 |

### 2.2 对 HTTP 家族的额外约束

- **流式与取消优先于“统一 buffer”。** SSE、gRPC streaming、WebSocket 和 AI token stream 都要求下游慢消费者背压、上游取消传播、半关闭/reset、心跳及正确的超时。普通 HTTP 的“请求结束即释放”模型不能覆盖 WebSocket 升级后的生命周期。
- **消息边界不等于网络 read 边界。** 一个 SSE event、gRPC message、WebSocket frame 或 JSON token 都可能跨多个 body chunk；一个 chunk 也可包含多个消息。任何消息感知 filter 都需要增量 codec。
- **协议转换不是无损默认操作。** H1/H2/H3 转换需要明确 trailers、 cancellation、flow control、Expect/upgrade、content length 与 end-of-stream 映射；gRPC 尤其不能只看 HTTP 200，最终状态可能在 trailers。

## 3. 三类不能化约为纯字节流的协议

### 3.1 QUIC / HTTP/3

**与 TCP 字节流的差异**

1. QUIC 的 socket 单位是 UDP 数据报，内部再提供多个有序、可靠 stream；还存在可选的不可靠 datagram 语义。只暴露 `AsyncRead/AsyncWrite` 会丢掉 stream 选择、reset、优先级和 datagram 等能力。
2. QUIC 将加密握手和传输状态紧密结合。透明 UDP relay 可以保持端到端加密，但无法读取 HTTP/3 header；终止 QUIC 则必须成为真正的加密传输端点。
3. Connection ID 与连接迁移使“固定五元组永久映射一个 upstream”不足。NAT rebinding、客户端换网或路径变化后，连接可能仍然存在；LB/relay 必须定义 CID 感知和粘滞策略，并设置合理的 idle timeout。
4. QUIC 减少的是跨 stream 的传输层队头阻塞，不消除应用背压、拥塞控制、丢包或网关调度延迟。

**HTTP/3 是另一层承诺**

- H3 在 QUIC stream 上承载 HTTP，并使用 QPACK。支持“QUIC 转发”不代表能按 H3 request 路由；支持“H3 终止”也不代表能代理私有 QUIC 协议。
- H3 终止后接 H1/H2 upstream 是可行架构，但它是**显式协议转换**：需验证 trailers、stream reset/cancel、flow control、0-RTT/重试风险和错误映射。
- 若仅需要让 QUIC 流量通过，应优先定义标准感知的 UDP relay/LB，不要伪装成 Pingora HTTP filter；若需要 HTTP 策略，则选择独立 H3 endpoint，再把 HTTP 语义适配到内部策略层。

**推荐支持口径**

- 第一阶段只承诺“HTTP/3 终止 + H1/H2 upstream”或“QUIC 透明 relay”中的一个，不要同时暗示两者。
- QUIC/H3 listener、连接迁移、CID 路由、丢包/重传、关闭和 0-RTT 均属于独立 PoC；**实施前需按锁定版本验证**所选 QUIC/H3 栈与 Pingora runtime 的集成方式。

### 3.2 MQTT 会话与 QoS

**透明 TCP 转发是合理且边界清晰的模式**

- 网关只在初始连接选择 broker，之后转发字节；MQTT session、subscription、QoS、Will、retained message 都由 broker 负责。
- 若 TLS 也端到端透传，网关无法可靠读取 ClientID 或 topic，因此不能声称基于这些字段路由或鉴权。
- 长连接建立后不能因普通负载均衡随意换 broker；断线重连是否必须回到持有会话的 broker，取决于 broker 集群能力和 MQTT session 配置。

**MQTT 终止或消息感知是另一个产品**

- 网关一旦分别终止客户端与 broker 的 MQTT 连接，QoS 就变成两段独立状态机。QoS 1 允许重复投递，QoS 2 的交互也不能因简单重发字节而保持语义；packet identifier 只在相应连接范围内有意义。
- 必须处理 CONNECT 鉴权、ClientID 冲突、Clean Start/Clean Session、Session Expiry、session-present、SUBSCRIBE/UNSUBSCRIBE、PUBACK/PUBREC/PUBREL/PUBCOMP、keepalive、Will、顺序与 disconnect。
- “upstream 断开后透明重连另一个 broker”可能丢失订阅、queued message、in-flight QoS 状态，甚至产生重复消息。除非有外部共享会话/可靠日志和明确的恢复协议，否则只能显式断线并交由客户端按 MQTT 语义恢复。
- MQTT over WebSocket 仍需先处理 WebSocket frame，再运行完整 MQTT codec/state machine；Pingora 能代理外层 WebSocket 并不构成 MQTT 支持。

因此，MVP 若只需 broker 前置 LB，应明确为 **TCP/TLS 透传**；topic ACL、按消息路由和多 broker 聚合应单独立项。

### 3.3 WebRTC：ICE/STUN/TURN、DTLS-SRTP 与 SCTP

**信令与媒体必须分开**

- HTTP/WebSocket/SSE 可以承载 SDP offer/answer、ICE candidate 等应用信令，Pingora 可代理这一部分。
- 信令成功不代表媒体可通。媒体路径要经历候选收集、ICE connectivity check/nomination、NAT 状态、DTLS 握手以及后续 SRTP/SCTP 流量。

**各组件不可被“UDP 字节流”替代**

- **STUN** 提供地址发现/绑定及 ICE 所需的连通性机制；它不是通用媒体 relay。
- **TURN** 是有 allocation、permission、channel、认证/凭证时效和生命周期的 relay 协议，不是把 UDP socket 随便转发到固定 upstream。
- **ICE** 需要在 host/server-reflexive/relay 等候选中检查并选择路径；NAT 类型、候选地址、路径切换和 TURN TCP/TLS fallback 都影响可达性。
- **DTLS-SRTP** 在媒体路径上建立密钥与保护 RTP/RTCP。透明 relay 保持端到端媒体加密，因而不能检查 SSRC 或执行媒体策略；SFU 终止每条腿的 DTLS-SRTP 后才有媒体可见性。
- **SCTP data channel** 具有 stream、消息边界、可靠/不可靠及有序/无序等配置，不能作为一个无边界 TCP 字节流处理。
- **SFU/媒体感知** 还涉及 RTP/RTCP、SSRC/MID、sequence number/timestamp、扩展、丢包重传、拥塞控制、抖动和码率反馈；这是实时媒体系统，不是 HTTP body filter。

**推荐支持口径**

分别声明“WebRTC 信令代理”“标准 TURN relay”“SFU/媒体转发”“Data Channel 终止”。前三者任何一项通过，都不能自动证明其他项。媒体面需要独立 UDP/TCP listener、allocation/session 状态、timer、NAT 与安全测试。

## 4. 对原关键工程主张逐条校正

### 4.1 “Everything is a Stream”——只能作为局部 I/O 抽象，不能作为统一领域模型

**成立条件**

- 在 HTTP body、TCP、部分 QUIC stream 中，用异步 chunk/stream 表达增量读写、背压和取消是有价值的。
- 可统一日志字段、字节计数、deadline、cancellation token 等横切机制。

**错误点**

- TCP byte stream、HTTP/2 stream、QUIC stream、WebSocket message、MQTT PUBLISH、SSE event 和 RTP packet 不是同义词。
- 纯流模型会丢失消息边界、QoS/session、trailer、datagram、RTCP 反馈、ICE candidate、连接迁移和 half-close/reset 等关键语义。
- 将所有输入强行转成字节流，往往会诱导读前缀、全量缓存或错误重试。

**修正建议**

采用分层抽象：公共控制面与可观测性 + 各数据面自己的 connection/message 类型。允许底层复用通用 I/O、buffer、timer 和 runtime，但领域 filter 必须看见相应协议语义。

### 4.2 “用 Pingora 微内核统一所有协议”——作为部署愿景可讨论，作为协议能力判断不成立

**成立条件**

- 若“微内核”仅指共享 Rust 进程、runtime、配置、服务发现、后台任务和指标，Pingora 的部分底层组件可能可复用。
- HTTP/H2 及基于 HTTP 的有限代理应由 `pingora-proxy` 承担。

**错误点**

- 官方快照把 `pingora-proxy` 定义为 programmable HTTP proxy，并未把它定义成任意协议插件内核。
- 共享 Tokio/Pingora runtime 不会自动生成 QUIC、MQTT 或 WebRTC 的握手、状态机与传输语义。
- 把所有协议放进同一故障域，还会放大热重载、内存、背压和升级的影响。

**修正建议**

统一配置 schema、身份/租户、策略决策、服务发现、审计和指标；至少划分 HTTP、TCP、QUIC/H3、MQTT、WebRTC 五类数据面。是否同进程是部署选择，不应改变协议边界。任何具体底层组件复用 **实施前需按锁定版本验证**。

### 4.3 “统一 GatewayFilter 直接接收 `pingora::proxy::Session`”——HTTP 内可行，跨协议不成立

**成立条件**

- 对 HTTP 请求链，统一 filter 可以执行鉴权、限流、路由、header/body 处理和观测。
- 外部快照中的 `Session` 是 established HTTP session，`ProxyHttp` 回调围绕 HTTP 请求阶段运行；这正说明它适合 HTTP，而不是通用连接对象。

**错误点**

- `Session` 的 HTTP cache、request/response、upstream peer 等概念无法表达 MQTT subscription、WebRTC allocation 或 QUIC migration。
- WebSocket 升级后的长连接和 gRPC 流也不能被简化为“一次性请求 filter 已执行，所以整个会话永久可信”。
- 具体 import 路径是否恰为 `pingora::proxy::Session`、回调签名与可见状态均 **实施前需按锁定版本验证**，不能把未锁定路径写成跨模块契约。

**修正建议**

- 公共策略输入使用协议中立的 principal、tenant、resource、action、deadline、trace context 等元数据。
- HTTP filter 接收 Pingora 类型；MQTT 使用 connect/publish/subscribe hook；WebRTC 使用 allocation/session hook；L4 使用 connection hook。
- 复用“策略决策器”，不要复用一个伪装成万能类型的 HTTP `Session`。

### 4.4 “AI 请求只读前 2～4KB，用 simd-json ondemand 提取 `model`/`stream`”——通用正确性不成立

**成立条件**

仅在网关自定义并强制执行的 schema 中，例如：只接受特定 JSON、禁止 content encoding、要求字段位于前缀、明确字段顺序与大小限制，超限/缺失即拒绝。此时 2～4KB 可以是**协议契约的一部分**，而非猜测。

**错误点**

1. JSON object 字段通常无顺序保证；`model` 可出现在 4KB 之后，尤其当前面是大型多模态 `messages` 时。合法 chunk 边界也可切断 key、escape 或 value。
2. 前缀内未找到字段不能推导为“字段不存在”；不完整 JSON 也不能直接当作恶意/格式错误。`stream` 缺省值、类型错误和重复 key 都需要明确策略。
3. gzip/br 等 content encoding、multipart、音频/二进制输入会让原始字节前缀根本不是 JSON。HTTP chunk 大小与 JSON 结构无关。
4. 无论 DOM 还是 on-demand parser，都不能从未读后缀恢复路由信息。SIMD 只可能改善解析成本，不能改变完整性、schema 或安全结论。
5. 若为了提前选 upstream 已经转发部分 body，后续发现真实 `model` 与初始路由冲突，将无法无损撤销。

**修正建议**

- 优先用可信的 API 路径、provider 凭据映射或显式内部路由元数据确定 provider；不要把客户端可伪造的便捷 header 当作唯一安全边界。
- 若路由必须依赖 body，使用**有上限、带 deadline 的增量解析和可重放缓冲**：找到全部路由所需字段后才决策；到 EOF/上限仍未满足则走明确默认或拒绝。大 body 需内存/磁盘上限和背压，不能无限 buffering。
- 2～4KB 可保留为 fast-path 大小，但必须有“继续读取”的正确路径，并保证转发 body 不丢字节、不重复、顺序不变。
- 对压缩、multipart、重复字段、未知字段和模型别名制定 schema；设置解压大小/比例限制。
- simd-json 的具体 ondemand API、可变性/生命周期、feature 与目标 CPU 行为 **实施前需按锁定版本验证**，并通过基准证明它确实是热点后再优化。

### 4.5 “Arc 原子指针替换实现热重载”——机制描述不完整

**成立条件**

- 将不可变配置构建成完整快照，用 `ArcSwap` 等专用原子 Arc 容器或受控锁/发布机制替换，能让新请求看到新快照，旧请求继续持有旧快照。
- 配置先完整解析、校验和构建依赖，再一次提交，可避免读者看到半更新状态。

**错误点**

- `Arc<T>` 是线程安全引用计数智能指针，不表示存放它的变量可以被多个线程原子替换。自行用裸 `AtomicPtr` 管理 Arc 会引入释放、ABA 和内存序风险。
- 替换配置不会自动更新已建立的 MQTT/WebRTC/WebSocket 会话、upstream 连接池、DNS 结果或 TLS 状态。
- Pingora 的 graceful reload/restart 不能自动等同于“所有配置与长连接原地热切换”；具体实现 **实施前需按锁定版本验证**。

**修正建议**

使用经验证的快照发布机制，不手写裸指针回收。按“prepare → validate → atomic publish → 新流量使用新 generation → 旧连接 drain/按策略迁移 → 回收”设计；路由、upstream、证书和 AI provider 配置应作为一致快照提交。失败可回滚到上一份完整快照，并监控旧 generation 滞留和资源泄漏。arc-swap 等第三方 API **实施前需按锁定版本验证**。

### 4.6 “在 Pingora 回调中通过 Hyper Flush 做微秒级 TTFT”——链路与指标均混淆

**成立条件**

- 网关应避免不必要的全量响应缓存，及时向下游写 header/body，并正确传播背压。
- 对 SSE/AI stream，可测试分段到达后是否立即触发下游写路径，必要时调整缓冲、压缩和聚合策略。

**错误点**

1. Pingora 已拥有自己的 HTTP proxy/I/O 路径；“存在一个通用 Hyper Flush，可从任意 Pingora 回调调用并保证客户端立即可见”没有依据。Hyper 是否存在、版本及任何可用集成 **实施前需按锁定版本验证**。
2. flush 只影响某一级写缓冲，不能控制上游模型排队/生成、TLS record、内核 socket、中间代理、网络 RTT、浏览器缓冲和客户端事件解析。过细 flush 还可能增加 syscall、H2 frame 和 TLS record 开销。
3. TTFT 通常指首个有效 token/内容，不是响应 header、SSE comment 或网关自己写入的字节。把本地一次 flush 的微秒耗时称为端到端 TTFT 会严重误导。

**修正建议**

- 不引入 Hyper 作为“flush 按钮”；沿锁定 Pingora 版本的 response header/body 写路径验证流式行为，具体回调和 flush 语义 **实施前需按锁定版本验证**。
- 禁止对 AI/SSE 流做全量缓存；检查缓存、压缩、响应转换及额外中间代理是否缓冲。按事件/合理 batch 写出，避免无节制的小包。
- 分开测量：上游 TTFT、网关附加延迟、下游首有效 token 时间，以及 p50/p95/p99；同时测吞吐、写放大和慢消费者。SLO 应由真实链路基线制定，不能预先承诺端到端微秒级。

### 4.7 “`upstream_fail` 后无缝 fallback”——只在很窄阶段可重试，流提交后不可能无缝

**框架事实边界**

所核对的 Pingora 官方快照使用 `fail_to_connect`、`error_while_proxy` 和 `fail_to_proxy` 等阶段，而不是以 `upstream_fail` 作为文档中的通用阶段；其 failover 指南明确说明：响应头发到 downstream 前可选择重试/换 upstream，发出后只能记录并结束。具体锁定版本中的回调名、可重试条件、body replay 和连接复用规则 **实施前需按锁定版本验证**。

**按失败阶段判断**

| 失败阶段 | 技术可行性 | 必须满足的条件 |
|---|---|---|
| 连接建立失败，确认未向上游发送请求 | 通常可选择另一个 peer | 重试预算、deadline、排除故障 peer、连接池/健康状态正确 |
| 已发送部分或全部请求，尚未收到/提交响应 | **执行状态未知** | 请求幂等或 provider 支持可靠 idempotency/deduplication；否则可能重复生成、扣费或产生副作用 |
| 收到上游响应但尚未向 downstream 提交 | 可在应用层谨慎放弃并换路 | 仍需处理上游已执行/计费、响应兼容、body 可重放及总 deadline |
| 已发送 downstream header、SSE event、gRPC message 或 token | **不能在原响应内透明重放** | 只能显式终止/错误、由客户端新请求恢复，或使用有应用协议支持的续传/去重机制 |

**AI 场景额外风险**

- AI 推理 POST 通常不能假定为幂等；跨 provider 的 idempotency key 也不一定通用。自动 fallback 可能产生两份生成结果和费用。
- 不同 provider/model 的上下文窗口、tool calling、token 计费、内容策略、数据驻留和响应格式未必兼容。所谓无缝 fallback 可能同时改变语义和合规边界。
- 已输出 token 后切换模型，无法保证文本、tool 状态和流式游标连续；隐藏切换会造成重复/缺失内容。

**修正建议**

把策略命名为 **bounded retry/failover with explicit commit semantics**，不要称“无缝”。默认仅在“确认未发送”或明确幂等时自动重试；设置 retry budget、指数退避/jitter、总 deadline、熔断与错误分类。跨 provider fallback 需要显式兼容矩阵、费用/合规策略和幂等设计；流提交后向客户端返回明确终止或通过新请求恢复。

## 5. 推荐的运行时边界

```text
统一控制面
  配置快照 / 服务发现 / 身份与租户 / 策略决策 / 配额 / 审计 / 指标与追踪
       │
       ├─ HTTP 数据面：Pingora
       │    HTTP/1、HTTP/2、HTTPS、SSE、AI HTTP、gRPC/WebSocket 的限定代理
       │
       ├─ L4 数据面：TCP/TLS 透传或终止后的字节 relay
       │
       ├─ QUIC/H3 数据面：UDP、CID/迁移、QUIC endpoint、可选 H3 转换
       │
       ├─ MQTT 数据面：codec、topic/QoS/session、broker 路由
       │
       └─ WebRTC 数据面：ICE/STUN/TURN、DTLS-SRTP/SCTP、媒体转发
```

- “统一”发生在管理、策略和观测层，而不是强制共用一个连接/session 类型。
- AI gateway 首先属于 HTTP 数据面；其模型路由、配额、provider 适配和 fallback 是有状态的领域逻辑，不应扩散成所有协议的通用 filter。
- 长连接数据面必须共享或各自实现背压、取消、超时、连接配额、draining 和关闭语义；不能仅做“字节已写入”的指标。
- 是否同进程可在 PoC 后决定。WebRTC 高 pps、MQTT 长会话与 HTTP 请求的故障/扩容特征不同，独立进程往往更容易隔离和伸缩。

## 6. 风险优先级

| 优先级 | 风险 | 影响 | 进入实现前的门槛 |
|---|---|---|---|
| **P0** | “支持协议”未区分 T/P/M，把 QUIC/MQTT/WebRTC 当作 HTTP stream | 架构返工、对外能力虚假、不可达或丢会话 | 为每种协议签署支持矩阵和明确不支持的语义 |
| **P0** | 2～4KB 前缀决定模型路由 | 错路由、配额/鉴权绕过、body 丢失或错误拒绝合法请求 | 增量解析、可重放缓冲、上限/deadline 及对抗测试通过 |
| **P0** | 非幂等 AI 请求自动 fallback，或响应提交后重试 | 重复生成/计费、重复或缺失 token、合规与模型语义变化 | 按失败阶段制定 commit/retry 矩阵，默认安全策略通过故障注入 |
| **P0（若纳入首版）** | MQTT QoS/session、QUIC migration、WebRTC NAT/媒体由普通代理逻辑承担 | 消息丢失/重复、连接中断、媒体完全不可达 | 各自独立 PoC；未通过则首版只承诺透明转发或信令，甚至移出范围 |
| **P1** | 流式背压、取消、超时、gRPC trailer、WebSocket 升级与慢消费者处理不完整 | 内存膨胀、连接泄漏、客户端悬挂、TTFT 抖动 | H1/H2 与长连接故障矩阵通过，资源指标稳定 |
| **P1** | 配置原子替换但长期连接和依赖未分 generation | 新旧配置混用、证书/路由不一致、旧连接永不释放 | 快照一致性、drain、回滚和泄漏测试通过 |
| **P2** | 过早进行 SIMD、flush、内核参数等微优化并追求“微秒 TTFT” | 增加复杂度而无端到端收益，甚至产生小包风暴 | 先建立正确性基线和 profile，再按数据决定优化 |

## 7. PoC 验证清单

### 7.1 版本与 API 基线（所有后续 PoC 的前置）

- [ ] 锁定 Rust 工具链、操作系统、Pingora/Hyper/simd-json/arc-swap 等依赖的精确版本、来源与 feature；Linux 作为重点验证环境。
- [ ] **实施前需按锁定版本验证** `ProxyHttp`/`Session` 实际路径与签名、body/response filter、H1/H2、TLS、WebSocket upgrade、gRPC trailer、retry/failover、连接池及 graceful reload 行为。
- [ ] 验证是否真的存在或需要 Hyper；若 Pingora 数据面不需要 Hyper，删除“Hyper Flush”设计假设。
- [ ] 将外部文档示例编译/运行于锁定版本，而非依据 `main` 分支 API 直接开发。

### 7.2 HTTP 与流式数据面

- [ ] 覆盖 downstream/upstream 的 H1/H2 组合，以及 H1↔H2 转换；验证 trailers、cancel/reset、client disconnect、half-close、idle/read/total timeout。
- [ ] 对 SSE 与 AI stream 使用“小 chunk、跨事件边界、延迟 token、心跳、长时间空闲、慢消费者”测试，确认无中间全量缓冲，背压不会导致内存无限增长。
- [ ] 对 gRPC 覆盖 unary、三种 streaming、压缩 message、Trailers-Only、非 200 HTTP 状态、`grpc-status` trailer、deadline 和取消。
- [ ] 对 WebSocket 分别验证锁定版本实际支持的 H1 Upgrade/H2 extended CONNECT、分片、ping/pong、close、大消息、压缩/子协议与升级后鉴权生命周期。
- [ ] 分别记录上游 TTFT、网关附加延迟、下游首有效 token 时间，及吞吐、写次数/写放大、CPU、内存；不以首个 header 代替首 token。

### 7.3 AI 请求解析与路由

- [ ] 将 `model`/`stream` 放在开头、4KB 边界前后、body 末尾；在 key、escape、Unicode、数字/布尔值中间切 chunk。
- [ ] 覆盖字段缺失、重复 key、错误类型、显式/缺省 `stream=false`、未知字段、模型别名、大小写及 malformed JSON。
- [ ] 覆盖 gzip/br、multipart、音频/二进制、大消息、多模态前置内容、超过上限、慢速上传和解压膨胀。
- [ ] 验证路由前缓冲能够**字节级准确重放**或按明确规则规范化；失败/重试时不丢、不重、不串请求。
- [ ] 验证无法确定路由时采取明确拒绝或受控默认，而非猜测；解析失败不能绕过模型配额、租户隔离或 provider 合规策略。

### 7.4 Retry / fallback 故障注入

- [ ] 分别注入：DNS/connect/TLS 失败、请求发送中失败、完整请求已发送但无响应、收到 header 后失败、首个 token 后失败、SSE 中途失败、429/5xx、timeout。
- [ ] 为每次故障记录上游是否可能执行、下游 commit 点、重试次数、总 deadline 与最终客户端结果。
- [ ] 使用可计数的 fake provider 验证非幂等请求默认不会重复执行；若声称幂等，验证 provider 真实去重与计费语义。
- [ ] 验证跨 provider 的模型能力、工具调用、数据驻留、配额和响应 schema 兼容；不兼容时禁止自动 fallback。
- [ ] 响应提交后验证客户端收到明确终止/可恢复错误，不出现被宣称为“无缝”的重复或缺失 token。

### 7.5 热重载与关闭

- [ ] 在高并发普通请求及 in-flight SSE/H2/WebSocket 下发布新快照，验证新请求一致使用新 generation，旧请求按策略完成或 drain。
- [ ] 同时变更 route/upstream/certificate/provider 配置，验证读者不会看到半更新状态；构造无效配置和回滚。
- [ ] 验证连接池、DNS cache、TLS 状态、后台任务及长期 MQTT/QUIC/WebRTC 会话的更新策略，不让“Arc 替换”被误认为全部生效。
- [ ] 检查旧配置、连接和 buffer 最终可回收；反复 reload 无 fd/内存/task 泄漏。

### 7.6 独立协议 PoC

**QUIC/H3（quick 待确认）**

- [ ] 先选择并写明“QUIC 透明 relay”或“H3 终止”；不要用一个测试替代另一个。
- [ ] 验证多 stream、stream reset/cancel、丢包/重传、idle timeout、连接关闭、Connection ID、NAT rebinding/迁移及 LB 粘滞。
- [ ] H3 场景验证 QPACK、trailers、H1/H2 upstream 转换、0-RTT 与重试副作用；压测 UDP 吞吐、pps、CPU 和丢包。

**MQTT**

- [ ] 明确 3.1.1/5、TCP/TLS/WebSocket 的支持范围，以及仅透传还是消息感知。
- [ ] 覆盖 QoS 0/1/2、DUP、packet identifier、顺序、retained message、Will、keepalive、ClientID 冲突、Clean Start/Clean Session、Session Expiry。
- [ ] 验证断线重连、broker 故障、session-present、in-flight 消息与 queued message；若无可证明的共享会话恢复，不得宣称 broker 间无缝 failover。

**WebRTC**

- [ ] 分开验收 HTTP 信令、STUN/ICE、TURN、DTLS-SRTP 媒体与 SCTP Data Channel。
- [ ] 在不同 NAT/防火墙下覆盖 host/server-reflexive/relay candidate、TURN UDP/TCP/TLS fallback、凭证过期、allocation/permission 生命周期和 ICE restart。
- [ ] 验证 DTLS fingerprint、SRTP 加解密、RTP/RTCP、丢包/抖动/重传/拥塞反馈，以及 data channel 的有序/可靠配置。
- [ ] 用真实浏览器/客户端互通并测量媒体可达率、建立时间、pps、带宽、延迟与资源消耗；HTTP 代理测试通过不算 WebRTC 通过。

## 8. 建议的落地顺序

1. **先冻结支持矩阵**：每种协议分别写 T/P/M、上下游版本、会话/重试语义及明确不支持项，并确认 quick 是否即 QUIC。
2. **第一里程碑**：Pingora HTTP/HTTPS/H1/H2 + AI SSE 流式代理；gRPC/WebSocket 仅承诺 PoC 已覆盖的模式；独立 TCP relay。
3. **第二里程碑**：有界 AI body 路由、幂等/故障阶段感知 retry、配置 generation 与 draining，完成故障注入和性能基线。
4. **独立里程碑**：QUIC/H3、MQTT 消息感知、WebRTC 各自 PoC 与运维模型通过后再接入统一控制面。若仅需 MQTT TCP 透传或 WebRTC 信令，可提前以该有限名称发布。

## 9. 参考材料

以协议标准和官方文档为主；Pingora 外部快照不替代锁定版本验证。

- Pingora README（上述快照的 `main`）：<https://github.com/cloudflare/pingora/blob/main/README.md>
- Pingora 请求阶段：<https://github.com/cloudflare/pingora/blob/main/docs/user_guide/phase.md>
- Pingora failover 与安全说明：<https://github.com/cloudflare/pingora/blob/main/docs/user_guide/failover.md>
- QUIC：RFC 9000 <https://www.rfc-editor.org/rfc/rfc9000>
- HTTP Semantics：RFC 9110 <https://www.rfc-editor.org/rfc/rfc9110>
- HTTP/2：RFC 9113 <https://www.rfc-editor.org/rfc/rfc9113>
- HTTP/3：RFC 9114 <https://www.rfc-editor.org/rfc/rfc9114>
- gRPC over HTTP/2：<https://github.com/grpc/grpc/blob/master/doc/PROTOCOL-HTTP2.md>
- WebSocket：RFC 6455 <https://www.rfc-editor.org/rfc/rfc6455>；H2 extended CONNECT：RFC 8441 <https://www.rfc-editor.org/rfc/rfc8441>
- SSE：WHATWG HTML Server-sent events <https://html.spec.whatwg.org/multipage/server-sent-events.html>
- MQTT 5.0：OASIS Standard <https://docs.oasis-open.org/mqtt/mqtt/v5.0/mqtt-v5.0.html>
- ICE：RFC 8445 <https://www.rfc-editor.org/rfc/rfc8445>
- STUN：RFC 8489 <https://www.rfc-editor.org/rfc/rfc8489>
- TURN：RFC 8656 <https://www.rfc-editor.org/rfc/rfc8656>
- DTLS-SRTP：RFC 5764 <https://www.rfc-editor.org/rfc/rfc5764>
- WebRTC Data Channels / SCTP over DTLS：RFC 8831 <https://www.rfc-editor.org/rfc/rfc8831>
