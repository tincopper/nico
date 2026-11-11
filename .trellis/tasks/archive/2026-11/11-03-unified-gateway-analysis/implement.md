# 后续 Rust 实施计划

> 阶段 0（Workspace 骨架、`ConfigSource` 契约、local 配置源、发布管线、Pingora 取证）已由子任务
> `archive/2026-11/11-03-phase0-tech-verification` 交付，证据见其 `research/phase0-capability-report.md`。
> 本文件描述阶段 1 及以后的工作顺序。etcd 不再是第一期前置条件。

## 1. 前置决策门

阶段 1 的两个门已由实际交付回答（provider 范围 = OpenAI/Anthropic/Gemini 四方言，部署 = 单进程双面）；
下面两项是**阶段 3 及以后**的待决门，随本任务归档保留在案：

- [x] `quick` 指 **QUIC**（2026-11-11 确认）。目标面仍未定：入站 HTTP/3、上游 HTTP/3、QUIC 转发
      （含 WebTransport）是成本相差一个量级的三件事，选定之前不进入该阶段。
      Pingora `0.9.1` 不含任何 QUIC / HTTP/3 支持（`pingora-core` 与 `pingora-proxy` 源码无相关符号，
      `Cargo.lock` 无 `quinn` / `h3`），所以该阶段必然引入新传输栈，不是打开一个 feature。
      它与上游 TLS 缺口正交：入站 HTTP/3 不解决「连不上 https provider」，只有上游 HTTP/3 顺带决定
      TLS 后端（届时只能是 rustls 一侧）。优先级低于上游 TLS。
- [ ] 确认第二期 LLM provider 范围、模型兼容范围、最大请求体、SLA 和部署环境。
- [x] 「依赖 body 的路由」策略已定：阶段 1 **不依赖 body**（vhost / path / 路由级 `provider` 字段）。
      "有界缓冲 + 单次重放"作为独立设计项在阶段 1 之后评审，不进入阶段 1 范围。
- [x] 配置源策略：第一期权威来源为 local 文件；etcd 推迟为后续里程碑（恢复条件见 §5）。
- [x] LKG 缓存策略：成功发布后始终写入；仅在 `allow_bootstrap_cache: true` 且启动 `load` 失败时读取，并标记 `stale`。
- [x] 锁定版本：Rust 1.99.0、edition 2024、MSRV 1.85、`tokio 1.54.1`、`serde_norway 0.9.42`、
      `notify 8.2.1`、`sha2 0.10.9`、`arc-swap 1.9.3`、Pingora `0.9.1`（仅 PoC，待阶段 1 进 workspace）。
- [x] 第一版默认静态链接和显式注册；第三方动态安装仍不作为默认机制。
- [x] 用户评审阶段 0 报告与本文后，再收敛阶段 1 范围（阶段 1 由 `11-04-s1-mvp` 的五个子任务按本文顺序交付并全部归档）。

## 2. 阶段 1：HTTP + LLM 协议转换 MVP（local 配置源）——已交付

由 `11-04-s1-mvp` 的五个子任务交付并全部归档（`archive/2026-11/`）：`11-04-s1-1-route-schema`、
`11-04-s1-2-http-data-plane`、`11-04-s1-3-nico-plugins`、`11-04-s1-4a-nico-llm-model`、
`11-05-s1-4b-nico-llm-protocols`。阶段 0 的六条实证约束全部按结论落地（h2c 显式、body 流式状态机、
graceful 显式配置、1xx 不插头、gRPC 显式 ALPN、不开启响应压缩）。

- [x] 建 `nico-http`：Pingora `0.9.1` 接入、路由匹配、upstream 选择、流式 body、SSE、h2c、graceful、1xx。
- [x] `nico-plugins`：JWT / API Key / Rate Limit，注册在 `nico-server`。
- [x] `nico-llm`：canonical schema、四个客户端协议 codec（OpenAI Chat/Responses、Anthropic、Gemini）与
      四个 provider adapter；`(protocol, provider)` 16 行矩阵 + golden fixture；核心无厂商 switch。
- [x] 路由 schema 扩展：`routes` / `upstreams` / `providers` 四段校验 + 文档嵌套深度与节点数上限。
- [ ] 重试与 fallback：**仍是阶段 2 范围**（MVP prd 的 Out of Scope 与设计 §10 一致：模型 fallback/熔断/
      重试预算在阶段 2）；本条未交付，进入阶段 2 时按"响应未提交且请求可重放"的约束重新取证。
- [x] 观测：阶段 1 以 `ConfigStatus` + `tracing` 字段暴露观测点；metrics exporter 留作独立任务。
- [x] 长连接与 drain：真实 SSE 在途请求的宽限期与旧 generation 回收有 e2e（`http_shutdown.rs`、`llm_gemini.rs`）。

**评审门**：新增 upstream/provider 不改 `nico-core`；普通 API 路由无 LLM 状态；任意 body 分片不丢字节；
reload 失败不影响旧流量；非 AI 路由不读 body；AI convert 路由的缓冲不得让上游挂起（请求头先发、body 在 EOS 后按 canonical 重新编码），阶段 0 实证的挂起问题不得复现。

**回滚点**：一旦发现路由需要读取 body 才能决定上游，停止实现并回到设计：该能力属于"有界缓冲 + 单次重放"设计项，不在路由实现里临时打补丁。

## 3. 阶段 2：Provider 与长寿命状态——部分交付，余项并入阶段 3

已交付（`11-08-s2-provider-state`，`c43195b`）：

- [x] 熔断、重试预算与显式兼容模型 fallback：目标身份级熔断（半开探测 + 跨 generation 继承）、
      `retry_budget` 内从规范化请求重编码后重试、`fallback` 链发布期物化且能力取交集。
      请求体超过 64 KiB（Pingora 重放缓冲区）后不可重试，是已记录的设计边界。
- [x] `SharedServices` 与配置 generation 的继承：进程级 breaker 表，配置未变的目标继承历史、
      改参的目标被丢弃（`crates/nico-server/src/llm/runtime.rs`）。
- [x] Cargo feature/role 构建：`traffic` 角色不编入 `nico-llm`，注册表为空，声明 provider 路由的
      文档在发布期被拒绝而不是静默跳过。
- [x] per-stream SSE 与 provider 错误事件：四个 adapter 各自折叠流，错误事件映射为类型化
      `ProviderError`。
- [x] 限流：以 `rate_limit` 插件形式随阶段 1 交付（令牌桶、有界表、跨 reload 保留），作用于请求粒度，
      不是 token 额度。

未交付，并入阶段 3：

- [x] **上游 TLS（最高优先）**：立项时构建拒绝所有 `https` 目标（`TLS_BACKEND_ENABLED = false`），
      因此所有真实 provider 都不可达、LLM 转换只在明文 mock 上被验证过。已由 `11-11-upstream-tls`
      关闭：`pingora/rustls` 后端已编入、`tls.ca_file` 可控信任根、真 TLS 上游的成功／不受信／
      主机名不匹配三条路径均有 e2e。
- [ ] 异构 Provider（非 HTTP/JSON 原生、或需要本地模型服务器语义的 kind）：语义待澄清。
- [ ] usage 归一化与计费：`schema::usage` 只定义了形状（provider 报数与网关计数分桶保留），
      provider 的 usage 按原样记录、不做归一化。
- [ ] LLM token 限额：与请求级限流是两件事，需要 usage 先有归一口径。
- [ ] 部署形态：role 只做到"可构建"，没有镜像、部署清单或远端 CI。

## 4. 阶段 3：按真实需求扩展

- [ ] 收取 §3 末的未交付项（上游 TLS、异构 Provider、usage 归一化与计费、token 限额、部署形态）。
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
- [x] 用户评审阶段 0 报告后归档本次分析任务（阶段 1 已按本文交付；阶段 2 待决门见 §1）。
