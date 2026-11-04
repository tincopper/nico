# 统一网关阶段 1：HTTP + AI 透传 MVP

## Goal

在阶段 0 骨架（`gateway-core` / `gateway-config` / `gateway-server`）上交付可运行的 HTTP + AI 透传网关：声明式路由与 upstream 模型、内置鉴权与限流、OpenAI 兼容 provider 透传与 SSE；配置来源仍是 **local 文件**；**路由不依赖请求 body**。

## 上游依据

- 阶段 0 结论与实证约束：`.trellis/tasks/archive/2026-11/11-03-phase0-tech-verification/research/phase0-capability-report.md`
- 架构设计：`.trellis/tasks/11-03-unified-gateway-analysis/design.md`
- 后端规范：`.trellis/spec/backend/`（尤其 §6 的 Pingora 实证约束）

阶段 0 留下的硬约束（不可违反）：

1. 明文 H2 必须显式 `h2c = true`，否则只按 H1 解析。
2. body 是流式块、边界不稳定；过滤器读完 body **不会**重放，上游会挂起 → 阶段 1 路由不读 body。
3. graceful 宽限期是固定 sleep，默认 300 s；必须显式配置。
4. 1xx 会进入 `upstream_response_filter`，不得无条件插入响应头。
5. 默认不加 `server` 头；上游默认 H1，gRPC 路由必须显式 `ALPN::H2`；不要开启响应压缩。

## Task Map

| 子任务 | 交付 | 依赖顺序 |
|---|---|---|
| `route-schema` | `routes` / `upstreams` / `providers` 配置模型、四段校验、文档边界 | 最先，其它三个都依赖它 |
| `http-data-plane` | `gateway-http`：Pingora 接入、路由匹配、upstream、流式 body、SSE、h2c、graceful、1xx | 依赖 `route-schema` |
| `gateway-plugins` | JWT / API Key / Rate Limit 内置 HttpFilter | 依赖 `http-data-plane` |
| `gateway-ai-passthrough` | `gateway-ai`：AI Filter、OpenAI Provider、按路由级 provider 选上游、SSE 透传 | 依赖 `http-data-plane`（可与 `gateway-plugins` 并行） |

子任务之间的顺序写在各子任务 `prd.md` 的 "Dependencies" 段，不靠树位置隐含。

## Cross-child Acceptance Criteria

- [ ] 五个基线命令全绿：`cargo +nightly fmt --all -- --check`、`cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`、`cargo test --workspace --all-features --locked`、`cargo deny check`、`RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`。
- [ ] 依赖方向不变：`gateway-core` 的依赖树无 `pingora` / `gateway-http` / `gateway-ai` / `gateway-config`；`gateway-http` 不 import `gateway-ai`。
- [ ] 一次 HTTP 请求的完整链路可跑通：local 配置 → 快照发布 → 路由匹配 → 上游转发 → 响应流式返回。
- [ ] 路由选择不读取请求 body（用"上游在 body 到达前已建立连接"的测试证明）。
- [ ] 配置热重载期间在途请求继续使用旧 generation；非法配置保留 LKG 且不中断服务。
- [ ] 所有新 crate 有 `//!` 文档、公开项 `///`、可失败函数 `# Errors`；新错误映射到 `ErrorCategory`。
- [ ] 新日志字段沿用 `config.generation` / `route_id` / `upstream` / `error.category`；不记录 body、请求头凭证或文档内容。
- [ ] 每个子任务的 Feature 影响写清；阶段 1 至少保证 `--no-default-features` 与 `--all-features` 可编译。

## Out of Scope

- "有界缓冲 + 单次重放"（按 `model` 路由）：独立设计项，阶段 1 之后评审。
- etcd / Nacos 接入：后续里程碑，恢复条件见阶段 0 报告 §8。
- Anthropic 等异构 Provider 的请求/响应语义转换、usage 归一化、模型 fallback（阶段 2）。
- metrics exporter：阶段 1 先以 `ConfigStatus` + `tracing` 暴露观测点，exporter 作为独立后续任务。
- MQTT / QUIC / WebRTC / TCP/L4 数据面、WASM 插件、动态加载。
- 管理 API（配置增删改）、多租户配额中心、审计存储。
