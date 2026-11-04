# 阶段 0：本地配置源骨架与 Pingora 能力验证

## Goal

建立可长期使用的 Rust Workspace 骨架与 `ConfigSource` 标准契约，先落地**本地文件配置源**，跑通「加载 → 校验 → 编译 → 不可变快照发布 → 热重载 → 失败保留 last-known-good」整条链路；同时对 Pingora HTTP 数据面能力做版本级取证。etcd 在本阶段完全不接入。

父任务：`.trellis/tasks/11-03-unified-gateway-analysis/`（架构分析）。

## Background and Confirmed Scope

- **第一阶段的配置权威来源改为本地文件。** `ConfigSource` 只定义契约，etcd 推迟为后续独立里程碑：本阶段不锁 `etcd-client` 版本、不启动 etcd 容器、不做 etcd 集成测试。
- 契约必须对 etcd 保持可接入性：来源内不透明 revision、仅完整快照事件、错误分类、adapter 吸收协议怪癖。这些约束在本阶段用 local + fake 实现验证，etcd 专属行为列入 deferred 清单。
- 仓库此前无 Rust 代码。本任务创建根 Workspace 与 `gateway-core`、`gateway-config`、`gateway-server` 三个 crate；`gateway-http`/`gateway-ai`/`gateway-plugins`/`gateway-config-etcd` 本阶段不创建。
- Pingora 行为仍在 `poc/` 中一次性验证，不进入 `crates/`。理由：回调与 body 语义尚未取证，先进骨架会把未验证形状变成事实契约。
- 已核实环境事实：`rustc/cargo 1.99.0` stable aarch64-apple-darwin；未安装 nightly 工具链与 `cargo-deny`；`curl 8.7.1` 含 nghttp2（支持 `--http2-prior-knowledge`）；crates.io sparse index 可访问。
- 已核实发布事实：`pingora 0.9.1`（Apache-2.0，`pingora-proxy` MSRV 1.85）、`tokio 1.54.1`、`notify` 稳定线 8.2.1（9.0 仍是 rc）、`serde_norway 0.9.42`、`sha2 0.10.9`/`0.11.1`、`async-trait 0.1.92`、`thiserror 2.0.21`、`arc-swap 1.9.3`。

## Requirements

### R1：版本与工具链锁定

- 锁定 Rust 工具链、edition/resolver、MSRV、`tokio`、`tokio-util`、`bytes`、`arc-swap`、`serde`、YAML crate、`sha2`、`notify`、`async-trait`、`thiserror`、`tracing`。
- 每项记录：版本、来源、许可证、MSRV、选择理由、被拒绝候选与原因。
- 禁止锁定预发布版本（`notify 9.0.0-rc.7`、`rustls 0.24.0-dev.2` 一律不选）。
- `etcd-client` **不进版本矩阵**，在报告 deferred 章节记录「恢复 etcd 时需要重新取证」。

### R2：Workspace 骨架与依赖边界

- 根 `Cargo.toml`（`[workspace]` + workspace 依赖 + workspace lints）、`rust-toolchain.toml`、`rustfmt.toml`、`clippy.toml`、`deny.toml`、`config/bootstrap.yaml`、`config/resources.yaml`。
- crate 边界：`gateway-core`（文档模型、generation、快照句柄、错误类别）、`gateway-config`（`ConfigSource` 契约、local 源、LKG 缓存、fake）、`gateway-server`（bootstrap、reload 管线、唯一 binary）。
- `gateway-core` 依赖树不得出现 `pingora`、`gateway-http`、`gateway-ai`、`gateway-config`、etcd/notify/sha2 之外的任何配置中心客户端；`gateway-config` 不得依赖 `gateway-server`。
- 五个基线命令必须通过：`cargo +nightly fmt --all --check`、`cargo clippy --workspace --all-targets --all-features -- -D warnings`、`cargo test --workspace --all-features`、`cargo deny check`、`RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`。

### R3：`ConfigSource` 标准契约

- 契约只有 `name` / `load` / `watch` 三个方法，`watch` 只产生 `Snapshot(ConfigVersion)` 与 `ResyncRequired` 两种事件，**不支持增量 patch**。
- `Revision` 来源作用域、可声明有序（`Comparable`）或仅可判等（`EqualityOnly`）；跨来源绝不比较。
- `ConfigSourceError` 按决策分类：`Unavailable` / `TooLarge` / `NotUtf8` / `RevisionRegression` / `ResyncRequired`，全部 `Send + Sync + 'static` 且有编译期断言。
- watch 必须 cancel-safe：drop 即取消订阅，不遗留后台任务或写端；输出通道有界，消费者落后时降级为 `ResyncRequired` 而非无界缓存。
- 契约不得出现任何来源专属类型（etcd key、Nacos dataId、notify 事件类型）。adapter 负责吸收：包含式 `start_revision`、fragment 聚合、key 删除、认证失败、协议重连。
- 摘要（digest）由管线统一计算，不由各 adapter 各自实现。

### R4：本地文件配置源

- 大小上限（默认 ≤ 1 MiB，bootstrap 可配）与超限拒绝；非法 UTF-8 明确报错。
- 变更检测：`notify`（监听配置路径所在目录，兼容 K8s symlink 切换）+ Unix `SIGHUP` 手动触发。
- 原子替换、半写入、文件删除、文件重建、symlink 指向切换均不得产生半快照或空快照。
- 文件删除/不可读表现为 `Unavailable`（配置不可用），不得编译成空配置。

### R5：发布管线

- 去抖（默认 200ms）+ 最大等待（默认 1s）、single-flight（同一时刻只有一个编译任务，其间只保留最新候选）。
- digest 去重：同一内容不重复编译；`Comparable` revision 回退事件丢弃；`EqualityOnly` 依赖 adapter 单连接顺序，管线只用 digest 与本地单调 generation。
- 发布：完整候选 `RuntimeSnapshot` 构建成功后才 `ArcSwap` 原子替换；失败保留 last-known-good 并标记 `degraded`。
- 就绪语义：首次成功发布前 `ready = false`；仅当 `allow_bootstrap_cache: true` 时才允许用 LKG 缓存启动并标记 `stale`。
- 在途读方持有 `Arc<RuntimeSnapshot>` 时，新一轮发布不得使其内容变化或失效。
- 停机：取消 watch 任务、等待管线退出，不中断在途请求持有的快照。

### R6：LKG 缓存

- 成功发布后写缓存：临时文件 + `fsync` + 原子 rename；元数据（revision、digest、generation、写入时间）与文档同目录分开保存。
- 读取时校验 digest 与元数据一致性；不一致视为损坏，**不得**静默降级为可用快照。
- 缓存写失败只告警，不影响已发布快照与服务。

### R7：Pingora HTTP 数据面取证（在 `poc/`）

- 对 H1/H2、body 钩子与分块、元数据晚到、SSE 增量与 flush、慢客户端背压、取消传播、连接池作用域、graceful 退出、WebSocket/gRPC 代理边界逐项给出「成立 / 有约束 / 不成立 / 未验证」结论。
- 结论分级：L0 源码 / L1 编译 / L2 运行时，禁止以文档推断代替实测。

### R8：报告与回写

- 报告：`.trellis/tasks/11-03-phase0-tech-verification/research/phase0-capability-report.md`，含契约就绪度、local 源验证、管线验证、Pingora 能力表、依赖边界、deferred 清单、对父任务的修正建议、复现步骤。
- 技术上可关闭的项写回父任务 `implement.md` 前置决策门；父任务中「etcd 为第一期权威来源」的表述需按 local-first 修正。
- 未决产品决策（`quick` 语义、SLA、provider 清单、最大请求体、LKG 冷启动）只给影响分析与建议值。

### R9：不得越界

- 不实现路由 DSL、`routes`/`upstreams`/`providers` 配置段、鉴权、限流、AI provider、Pingora HTTP 服务接入。
- 不创建 etcd/Nacos crate，不锁 `etcd-client`，不启动容器。
- 不引入 metrics exporter（本阶段以 `ConfigStatus` + `tracing` 暴露观测点，指标落地列为 deferred）。
- 不发布吞吐/延迟/内存数值结论；PoC 代码不迁入 `crates/`。

## Acceptance Criteria

- [x] 版本矩阵成文（版本、来源、许可证、MSRV、拒绝项理由），无预发布锁定，`etcd-client` 明确不在矩阵内。
- [x] 五个基线命令通过；`cargo +nightly fmt --check` 使用 nightly 工具链（若安装失败，报告记录偏差与替代门禁）。
- [x] `cargo tree -p gateway-core` 无 `pingora|gateway-http|gateway-ai|gateway-config|etcd|notify` 输出；`cargo tree -p gateway-config` 无 `gateway-server`。
- [x] `ConfigSource` 契约成文且有 doc test；错误类型有 `Send + Sync + 'static` 编译期断言。
- [x] local 源测试覆盖：原子 rename、K8s symlink 切换、半写入、文件删除与重建、超限、非法 UTF-8、SIGHUP 触发。
- [x] 管线测试覆盖：去抖与最大等待（`tokio::time::pause`）、single-flight、digest 去重、`Comparable` 回退丢弃、`EqualityOnly` 顺序处理、失败保留 LKG、修复后成功发布、首次发布前 not-ready、缓存 stale 启动。
- [x] 并发验证：发布新代时持有旧 `Arc<RuntimeSnapshot>` 的读方内容不变。
- [x] 缓存测试覆盖：原子写、元数据校验、digest 不匹配视为损坏、写失败仅告警。
- [x] `Document` 解析拒绝未知字段、错误 `apiVersion`、越界 limits；任意字节输入不 panic（proptest）。
- [x] Pingora 能力表九项均有结论与证据，未验证项显式标注原因。
- [x] SSE 任意字节拆包、慢客户端、客户端断连三类场景有实测记录。
- [x] 大 body（> 元数据窗口）转发不丢字节、不改顺序。
- [x] 报告含 deferred 章节：etcd 恢复时必须重新取证的 6 项，以及契约是否需要扩展的判定。
- [x] 未创建 `gateway-http`/`gateway-ai`/`gateway-plugins`/`gateway-config-etcd`，未锁 `etcd-client`，未引入 metrics exporter。

## Out of Scope

- etcd / Nacos 接入与验证；多 key 一致性；配置中心容器集成测试。
- 路由 DSL、`routes`/`upstreams`/`providers` 配置段、JWT/限流、AI provider、SSE 协议转换。
- `gateway-http` 骨架与 Pingora listener 接入；`gateway-ai`/`gateway-plugins`。
- metrics exporter、CI、部署清单、发布流水线、性能基准。
- MQTT、QUIC/HTTP3、WebRTC 任何实现或验证；`quick` 语义确认。

## Open Product Decisions（不由本任务关闭）

1. `quick` 是否指 QUIC，目标是 HTTP/3 入口、QUIC 代理还是 WebTransport。
2. 第一期 AI provider 清单、模型兼容范围、最大请求体、SLA、部署环境。
3. 生产环境是否允许 `allow_bootstrap_cache: true`（local-first 下意义有限，恢复远端来源后才关键）。
4. etcd 恢复时间点与是否仍作为生产权威来源。
