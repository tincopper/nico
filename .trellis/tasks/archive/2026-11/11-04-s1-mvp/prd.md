# 统一网关阶段 1：HTTP + LLM 协议转换 MVP

## Goal

在阶段 0 骨架（`nico-core` / `nico-config` / `nico-server`）上交付可运行的 HTTP + LLM 协议转换网关：声明式路由与 upstream 模型、内置鉴权与限流、canonical LLM 模型与客户端协议 codec/provider adapter（OpenAI Chat/Responses、Anthropic、Gemini）、SSE；配置来源仍是 **local 文件**；**路由选择不依赖请求 body**，AI convert 路由在路由确定后有界缓冲 body。

## 上游依据

- 阶段 0 结论与实证约束：`.trellis/tasks/archive/2026-11/11-03-phase0-tech-verification/research/phase0-capability-report.md`
- 架构设计：`.trellis/tasks/11-03-unified-gateway-analysis/design.md`
- 后端规范：`.trellis/spec/backend/`（尤其 §6 的 Pingora 实证约束）

阶段 0 留下的硬约束（不可违反）：

1. 明文 H2 必须显式 `h2c = true`，否则只按 H1 解析。
2. body 是流式块、边界不稳定；过滤器读完 body **不会**重放，上游会挂起 → 路由选择不读 body；协议转换所需的 body 在路由确定后由 AI convert 路由有界缓冲，canonical 可重放，不依赖 Pingora 重放原始 body。
3. graceful 宽限期是固定 sleep，默认 300 s；必须显式配置。
4. 1xx 会进入 `upstream_response_filter`，不得无条件插入响应头。
5. 默认不加 `server` 头；上游默认 H1，gRPC 路由必须显式 `ALPN::H2`；不要开启响应压缩。

## Task Map

| 子任务 | 交付 | 依赖顺序 |
|---|---|---|
| `1.1 route-schema` | `routes` / `upstreams` / `providers` 配置模型、四段校验、文档边界 | 最先；`1.2`、`1.3`、`1.4a` 依赖它 |
| `1.2 http-data-plane` | `nico-http`：Pingora 接入、路由匹配、upstream、流式 body、SSE、h2c、graceful、1xx | 依赖 `1.1` |
| `1.3 nico-plugins` | JWT / API Key / Rate Limit 内置 HttpFilter | 依赖 `1.2` |
| `1.4a nico-llm-model` | `nico-llm`：canonical schema、OpenAI Chat codec/provider、AI 路由有界请求缓冲与流式响应转换缝 | 依赖 `1.2`；`1.4b` 依赖它 |
| `1.4b nico-llm-protocols` | `nico-llm`：Anthropic Messages / Responses / Gemini 的客户端 codec 与 provider adapter；`(protocol, provider)` 矩阵 | 依赖 `1.4a` |

子任务之间的顺序写在各子任务 `prd.md` 的 "Dependencies" 段，不靠树位置隐含。

## Cross-child Acceptance Criteria

- [x] 五个基线命令全绿：`cargo +nightly fmt --all -- --check`、`cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`、`cargo test --workspace --all-features --locked`（1626 passed / 55 suites）、`cargo deny check`、`RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`。
- [x] 依赖方向不变：`nico-core` 的依赖树无 `pingora` / `nico-http` / `nico-llm` / `nico-config`；`nico-http` 不 import `nico-llm`（`cargo tree` 逐项无输出；`nico-llm/tests/structure.rs` 锁定）。
- [x] 一次 HTTP 请求的完整链路可跑通：local 配置 → 快照发布 → 路由匹配 → 上游转发 → 响应流式返回（`nico-server/tests/local_file.rs`、`http_*.rs`、`llm_*.rs`）。
- [x] 路由选择不读取请求 body；AI convert 路由在路由确定后有界缓冲 body（`request.max_body_bytes`），canonical 可重放；非 AI 路由不读 body（`hooks` 契约测试与 e2e 断言）。
- [x] canonical 是客户端 codec 与 provider adapter 之间唯一的公共词汇；`schema/` 与 `filter.rs` 无厂商 switch；`(protocol, provider)` 矩阵有 golden fixture（`nico-llm/tests/matrix/` 16 行 + 每方言 4 份 wire fixture，1.4b 第 9 步）。
- [x] 响应转换逐块进行（SSE 不整块），非流式响应受 `request.max_response_bytes` 约束；`nico-http` 不依赖 `nico-llm`（transform 测试 + `cargo tree`）。
- [x] 配置热重载期间在途请求继续使用旧 generation；非法配置保留 LKG 且不中断服务（`llm_publication.rs`、`plugins_config.rs`、`schema_bounds.rs`）。
- [x] 所有新 crate 有 `//!` 文档、公开项 `///`、可失败函数 `# Errors`（workspace `missing_docs` deny + clippy）；新错误映射到 `ErrorCategory`。
- [x] 新日志字段沿用 `config.generation` / `route_id` / `upstream` / `error.category`；不记录 body、请求头凭证或文档内容（logging spec + e2e 记录断言）。
- [x] 每个子任务的 Feature 影响写清；阶段 1 至少保证 `--no-default-features` 与 `--all-features` 可编译（`cargo check --no-default-features --workspace` 通过）。

## Out of Scope

- 按请求 body 里的 `model` 选上游、以及为原始 body 做"有界缓冲 + 单次重放"：独立设计项，阶段 1 之后评审；1.4a 的缓冲只用于协议转换，canonical 本身可重放。
- etcd / Nacos 接入：后续里程碑，恢复条件见阶段 0 报告 §8。
- usage 归一化/计费、模型 fallback、熔断（阶段 2）；Anthropic/Responses/Gemini 的协议转换在 `1.4b`。
- metrics exporter：阶段 1 先以 `ConfigStatus` + `tracing` 暴露观测点，exporter 作为独立后续任务。
- MQTT / QUIC / WebRTC / TCP/L4 数据面、WASM 插件、动态加载。
- 管理 API（配置增删改）、多租户配额中心、审计存储。
