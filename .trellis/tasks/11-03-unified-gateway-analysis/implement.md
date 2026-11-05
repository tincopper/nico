# 后续 Rust 实施计划

> 阶段 0（Workspace 骨架、`ConfigSource` 契约、local 配置源、发布管线、Pingora 取证）已由子任务
> `archive/2026-11/11-03-phase0-tech-verification` 交付，证据见其 `research/phase0-capability-report.md`。
> 本文件描述阶段 1 及以后的工作顺序。etcd 不再是第一期前置条件。

## 1. 前置决策门

- [ ] 确认 `quick` 是否指 QUIC，以及目标是 HTTP/3、QUIC proxy 还是 WebTransport；未确认前不进入该阶段。
- [ ] 确认第一期 LLM provider、模型兼容范围、最大请求体、SLA 和部署环境。
- [x] 「依赖 body 的路由」策略已定：阶段 1 **不依赖 body**（vhost / path / 路由级 `provider` 字段）。
      "有界缓冲 + 单次重放"作为独立设计项在阶段 1 之后评审，不进入阶段 1 范围。
- [x] 配置源策略：第一期权威来源为 local 文件；etcd 推迟为后续里程碑（恢复条件见 §5）。
- [x] LKG 缓存策略：成功发布后始终写入；仅在 `allow_bootstrap_cache: true` 且启动 `load` 失败时读取，并标记 `stale`。
- [x] 锁定版本：Rust 1.99.0、edition 2024、MSRV 1.85、`tokio 1.54.1`、`serde_norway 0.9.42`、
      `notify 8.2.1`、`sha2 0.10.9`、`arc-swap 1.9.3`、Pingora `0.9.1`（仅 PoC，待阶段 1 进 workspace）。
- [x] 第一版默认静态链接和显式注册；第三方动态安装仍不作为默认机制。
- [ ] 用户评审阶段 0 报告与本文后，再收敛阶段 1 范围。

## 2. 阶段 1：HTTP + LLM 协议转换 MVP（local 配置源）

- [ ] 建 `nico-http`：Pingora `0.9.1` 接入、`HttpFilter`、`RouteTable`/`CompiledRoute`、upstream 选择。
      落地时必须按阶段 0 结论实现：明文 H2 显式 `h2c = true`；body 按流式状态机处理，跨块保存残包；
      graceful 宽限期显式配置（默认 300 s 是固定 sleep，不适用于长 SSE）；1xx 响应不插入响应头；
      gRPC 路由显式 `ALPN::H2`；不要开启响应压缩。
- [ ] `nico-plugins`：JWT / API Key / Rate Limit，注册在 `nico-server`。
- [ ] `nico-llm`：canonical schema、客户端协议 codec、provider adapter、AI 路由有界 body 与流式响应转换。
      1.4a 先冻结形状并完成 OpenAI Chat；1.4b 补 Anthropic/Responses/Gemini 的 `(protocol, provider)` 矩阵。
      核心不按厂商 switch；转换只发生在 `protocol/` 与 `provider/`。
- [ ] 路由 schema 扩展：在 `Document` 中加入 `routes` / `upstreams` / `providers`（路由级 `provider` 字段用于选上游），并落地
      解析 → 引用校验 → 语义校验 → 构造四段校验；同批加入文档嵌套深度与节点数上限（当前只有 `max_bytes`）。
- [ ] 重试与 fallback：只允许"响应未提交且请求可重放"的受限重试；SSE 首字节后不得透明换源。
- [ ] 观测：阶段 1 以 `ConfigStatus` + `tracing` 字段暴露观测点；metrics exporter 作为阶段 1 之后的独立任务，不在本阶段引入依赖。
- [ ] 长连接与 drain：用真实 SSE/H2 在途请求验证宽限期配置与旧 generation 回收。

**评审门**：新增 upstream/provider 不改 `nico-core`；普通 API 路由无 LLM 状态；任意 body 分片不丢字节；
reload 失败不影响旧流量；非 AI 路由不读 body；AI convert 路由的缓冲不得让上游挂起（请求头先发、body 在 EOS 后按 canonical 重新编码），阶段 0 实证的挂起问题不得复现。

**回滚点**：一旦发现路由需要读取 body 才能决定上游，停止实现并回到设计：该能力属于"有界缓冲 + 单次重放"设计项，不在路由实现里临时打补丁。

## 3. 阶段 2：Provider 与长寿命状态

- [ ] 在 `nico-llm::provider` 新增异构 Provider，复用 HTTP upstream。
- [ ] per-stream SSE/Provider 状态、错误事件与 usage 归一化。
- [ ] 限流、熔断、重试预算与显式兼容模型 fallback。
- [ ] `SharedServices` 与配置 generation 的继承、drain 和回收。
- [ ] 用 Cargo feature/role 构建并部署 traffic 与 llm 两种组合。

## 4. 阶段 3：按真实需求扩展

- [ ] 新协议使用独立 crate/binary 和协议原生 trait，不穿透 HTTP Filter。
- [ ] 出现第二个真实消费者后，才下沉公共 trait 或拆分新 crate。
- [ ] 第三方动态插件若成为需求，独立评估 WASM/进程隔离、签名、权限、资源和背压。

## 5. etcd 里程碑（deferred）

恢复 etcd 前必须重新取证，任一项不通过不得进入生产：

- [ ] `etcd-client` 锁定版本的 Watch API（`created` / `canceled` / `compact_revision` / `events` / `fragment`）。
- [ ] 包含式 `start_revision`：`load` 得 `R` 后从 `R + 1` 订阅不重不漏。
- [ ] compaction / watcher 落后：watch 被取消并带 `compact_revision`，adapter 触发 `ResyncRequired`。
- [ ] fragment：按 watch_id 聚合或显式拒绝并全量重载。
- [ ] Auth 与 TLS；认证失败与 key 删除映射为 `Unavailable`（不是空配置）。
- [ ] endpoint failover；`cargo tree -d` 检查 tonic/hyper/TLS 是否与 Pingora 形成第二套栈。
- [ ] 单 key 完整文档的大小上限与 `max_bytes` 预算（etcd 3.5 默认 `max-request-bytes` ≈ 1.5 MiB）。

契约不需要为 etcd 扩展；若上述取证表明协议怪癖无法在 adapter 内吸收，再单独评审 `ConfigEvent` 扩展。

## 6. 验证命令（现状）

```bash
# 产品 workspace（当前 127 个测试）
cargo +nightly fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo deny check
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

# 依赖边界（应无输出）
cargo tree -p nico-core | grep -E 'pingora|nico-http|nico-llm|nico-config|notify|etcd'
cargo tree -p nico-config | grep -E 'nico-server|pingora|etcd'

# 真实文件热重载
cargo run -p nico-server -- --bootstrap config/bootstrap.yaml

# Pingora 一次性取证（独立 workspace）
cd poc && cargo build --all-features && ./scripts/run-probes.sh
```

阶段 1 起追加：`nico-http` 的 route/filter/streaming 测试、feature 矩阵（`--no-default-features` / `--features llm`）、
长连接 drain 与负载测试、真实 etcd 集成测试（etcd 里程碑）。

## 7. 当前分析任务检查

- [x] 给出 Rust 微内核职责、crate 边界、模块划分和依赖方向（阶段 0 已按此落地 3 个 crate）。
- [x] 给出 `ConfigSource` 契约、local 实现、revision/digest/去抖/single-flight/LKG 设计并实现。
- [x] 给出 Pingora 能力取证与阶段 1 约束（H1/H2、body、SSE、背压、取消、池、graceful、WS/gRPC）。
- [x] 给出 deferred etcd 清单与恢复条件。
- [ ] 用户评审阶段 0 报告后归档本次分析任务。
