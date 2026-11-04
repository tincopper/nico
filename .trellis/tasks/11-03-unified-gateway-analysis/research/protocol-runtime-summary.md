# 协议与运行时约束摘要

> 本文件是供实施/检查上下文加载的精简摘要。完整证据、协议矩阵、风险与 PoC 清单见 `protocol-and-runtime-boundaries.md`；HTTP/AI 工程评审见 `http-ai-implementation-review.md`；etcd 配置源一手资料见 `config-source-options.md`。

## 版本证据边界

- 产品 workspace 已锁定依赖（Rust 1.99.0、edition 2024、MSRV 1.85、`tokio 1.54.1`、`serde_norway 0.9.42`、
  `notify 8.2.1`、`sha2 0.10.9`、`arc-swap 1.9.3`），`Cargo.lock` 已提交。
- Pingora 已按 `0.9.1` 完成 HTTP 数据面取证（H1/H2、body 钩子、SSE、背压、取消、连接池、graceful、
  WebSocket/gRPC 边界），结论与源码行号见任务 `11-03-phase0-tech-verification/poc/FINDINGS.md`。
- 阶段 1 必须遵守的实证约束：明文 H2 需显式 `h2c`；body 为流式块、边界不稳定；过滤器读完 body 不会重放；
  graceful 宽限期是固定 sleep（默认 300 s）；1xx 会进入响应过滤器；默认不加 `server` 头，上游默认 H1。
- WebRTC 已由用户确认暂缓，不进入当前架构或实施范围；`quick` 暂按潜在 QUIC 笔误处理，仍待确认。

## 核心协议边界

- 统一控制面与策略机制，不统一所有协议 Session。Connection、Stream、Message、Request、Datagram 不可混为一谈。
- 每种协议分别声明透明转发、协议终止、消息感知和转换能力；能代理外层协议不等于理解内部消息。
- Pingora HTTP 插件承载 HTTP/1、HTTP/2、HTTPS、SSE，以及经过验证的 gRPC/WebSocket 代理模式。
- gRPC 透明代理不等于 message codec、RPC 重试、反射或转码；必须验证 trailers、deadline、cancel 和 H2 flow control。
- WebSocket Upgrade 后具有独立 frame/message 生命周期；普通 HTTP body hook 不等于消息感知。
- SSE 是 HTTP 流式响应；事件/行/UTF-8/JSON 可跨 Chunk，解析器必须保存每流状态并服从背压。
- TCP 使用独立 L4 数据面；MQTT over TCP/TLS/WebSocket 还有 QoS、session、Keepalive 等状态，不能复用 HTTP Session。
- QUIC 是 UDP 上的独立传输，HTTP/3 是其应用映射；多流、CID、迁移和 datagram 需要独立引擎与 PoC。

## HTTP/AI 正确性约束

1. 不得假设 `model`/`stream` 位于前 2～4KB；字段可乱序、重复、缺失或位于大 `messages` 后。
2. `simd-json ondemand` 不能直接视为不完整网络前缀解析保证。若路由依赖 body，使用有界增量扫描和可重放缓冲；超限明确拒绝或受控默认。
3. body 可能压缩、multipart、任意分块；必须限制 body、解压、内存和解析 deadline，保证原始字节不丢、不重、顺序不变。
4. Pingora 响应头与响应体阶段分开；不预设 Hyper Flush，不承诺端到端微秒级 TTFT。
5. 慢客户端必须传播背压或取消；不得使用无界队列，不能为追求 flush 产生无界内存或小包风暴。
6. `Arc` 不是原子配置槽。使用安全的不可变快照发布，配置 prepare 成功后 commit，失败保留 last-known-good，旧请求/流 drain。
7. 配置热重载不等于二进制插件热更新；连接池、DNS、TLS 和长连接拥有各自生命周期。
8. AI 推理不默认可重试。请求已发送但响应未收到时执行状态未知，可能重复生成和计费；响应或 token 提交后不得透明 fallback。
9. 跨 Provider fallback 必须验证模型能力、工具调用、成本、合规、响应 schema 和幂等，不得把模型当成普通 IP 备份。
10. usage 区分 Provider 返回值与本地估算；默认不记录 prompt、response、Authorization 或 Provider secret。

## 配置源约束（第一期 local；etcd/Nacos 为后续里程碑）

1. `ConfigSource` 只负责 `load` + `watch` 完整文档，不解析路由、不直接修改运行快照；server 统一去抖、single-flight、校验、编译和 ArcSwap 发布。
2. `Revision` 是来源内不透明版本（etcd `mod_revision`、Nacos 内容版本/MD5），绝不跨来源比较；MD5 只能判等不能排序时，adapter 保证单连接顺序，server 用本地 generation 单调递增。
3. 默认不支持增量 patch；多 key 必须全量重读，全部必选 key 可读且完整后才发布，否则保留 LKG 并标记 degraded。
4. watch 必须可取消、有界、可重连；慢消费者触发 `ResyncRequired`，不能无界缓存。断线后先 `load` 再继续 watch；compaction/gap 强制全量重载。
5. 同一 digest 不重复编译；可比 revision 回退/乱序丢弃，不可比 token 由 adapter 保证单连接顺序；去抖 + 最大等待避免 reload 风暴和饥饿。
6. 启动配置源（类型、地址、认证引用、缓存、上限）只能来自本地 bootstrap，不能来自配置中心自身。
7. 启动无快照默认 not ready；仅显式允许时可用 LKG 缓存启动并标记 stale。运行期源不可用时继续用 LKG 并告警。
8. 配置中心是特权输入：TLS/认证/RBAC、大小/深度/数量/编译时间上限、未知字段拒绝、禁止任意文件/命令路径、secret 仅存引用。
9. 跨实例只保证最终一致，不承诺集群原子切换；路由变更先加后删，观测暴露 revision/digest/degraded。
10. 第一期只交付 local 文件源，并已实现以下行为：大小上限、非法 UTF-8、原子 rename、K8s symlink 切换、半写入、删除后保留 LKG、SIGHUP；etcd/Nacos 保留扩展位，不进入当前验收。
11. etcd 里程碑（deferred）：`start_revision` 是包含式，`load` 得到 R 后从 `R + 1` watch；compaction 或 watcher 落后时响应带 `compact_revision`，必须全量重载；默认单 key 完整文档。
12. etcd 里程碑（deferred）：release-3.5 默认 `max-request-bytes = 1.5 * 1024 * 1024`；`max_bytes` 不超过 1 MiB。大 revision 可能 fragment，adapter 需聚合或显式全量重载；key 删除/认证失败视为不可用而非空配置。
13. etcd 里程碑（deferred）：候选客户端 `etcd-client` 0.21.x 基于 tonic/prost，Watch 是低层双向流，需自行处理 created/canceled/compact/events/fragment；必须用 `cargo tree -d` 检查与 Pingora 的 tonic/hyper/TLS 依赖冲突。恢复条件清单见阶段 0 报告 §8。

## 最小 PoC 集合

- H1/H2 组合、trailers、cancel/reset、half-close、idle/total timeout、慢消费者。
- `model` 位于 4KB 后/body 末尾、重复/缺失/错误类型、压缩、multipart、大 body、任意分片与准确重放。
- SSE 在任意字节位置拆包，含 CRLF、多行 data、心跳、错误事件和 `[DONE]`。
- 在连接、发送中、发送完成、响应头、首 token、流中途分别注入失败，验证 commit/retry 安全和重复计费风险。
- 高并发与 in-flight SSE/H2 下配置发布、回滚、旧代回收；重复 reload 无 fd/task/buffer 泄漏。
- WebSocket/gRPC 只发布 PoC 实际覆盖的代理模式；MQTT/QUIC 后续按原生协议语义独立验收。
- 用 FakeConfigSource 覆盖乱序、重复、断线、gap、慢消费者、超大/非法文档、编译失败与 LKG；第一期真实 etcd 集成测试验证认证、TLS、watch 重连、compaction 和 endpoint failover。Nacos 集成测试在其后续实现时补充。
