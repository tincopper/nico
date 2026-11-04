# 阶段 0 能力确认报告

> 范围：本地配置源骨架（`gateway-core` / `gateway-config` / `gateway-server`）+ Pingora 0.9.1 HTTP 数据面取证。
> 一次性 Pingora 证据在同目录 `poc/FINDINGS.md`；本文给出结论、证据等级与对父任务的修正。
> 未验证项全部集中在 §8，不在正文隐藏。

## 1. 结论摘要

| 项 | 结论 | 证据等级 |
|---|---|---|
| 版本与工具链锁定 | 成立 | 构建可复现（`Cargo.lock` + `rust-toolchain.toml`） |
| Workspace 骨架与依赖方向 | 成立 | L1 + `cargo tree` 无越界输出 |
| `ConfigSource` 契约（load/watch/revision/digest/有界/取消） | 成立 | L1 + 127 个测试 |
| 本地文件源（原子 rename / symlink / 删除重建 / SIGHUP） | 成立 | L2（tempdir 实测） |
| 发布管线（去抖 / 最大等待 / single-flight / digest 去重 / 顺序 / LKG / ready-stale） | 成立 | L2（paused time + 真实文件 e2e） |
| LKG 缓存（原子写 / meta 校验 / 损坏检测） | 成立 | L1 |
| 真实文件热重载端到端 | 成立 | L2（手工 run，见 §5.3） |
| Pingora H1/H2、body 钩子、SSE、背压、取消、连接池、大 body | 成立 | L2（见 §6） |
| Pingora graceful 退出 | 有约束：宽限期是固定 sleep，默认 300 s | L2 + L0 |
| Pingora 依赖 body 的路由 | 不成立（朴素实现）：过滤器读完 body 不会重放，上游挂起 | L2 |
| WebSocket / gRPC | 有约束：H1 WS 字节转发、H2 gRPC（unary/服务端流）成立；WS over H2 不支持 | L2 + L0 |
| etcd 配置源 | 未验证（本阶段推迟，见 §8） | 未验证 |

## 2. 版本矩阵

| 组件 | 锁定版本 | 来源 | 许可证 | MSRV | 选择理由 |
|---|---|---|---|---|---|
| Rust | 1.99.0（`rust-toolchain.toml`） | rustup stable | — | — | 与 nightly 并存的 fmt 门禁 |
| edition / resolver | 2024 / `3` | — | — | 1.85 | 与 spec 的 `resolver = "3"`（edition 2024）一致 |
| `tokio` | `=1.54.1` | crates.io | MIT | 1.85 | `fs`/`signal`/`time`/`sync` 全部需要 |
| `tokio-util` | `=0.7.20` | crates.io | MIT | 1.85 | `CancellationToken` |
| `bytes` | `=1.12.1` | crates.io | MIT | 1.57 | 文档字节 |
| `arc-swap` | `=1.9.3` | crates.io | MIT OR Apache-2.0 | — | `ArcSwapOption<RuntimeSnapshot>` |
| `serde` | `=1.0.229` | crates.io | MIT OR Apache-2.0 | 1.56 | `deny_unknown_fields` 必需 |
| `serde_norway` | `=0.9.42` | crates.io | MIT OR Apache-2.0 | 1.71 | `serde_yaml` 的维护分支；已实测 `deny_unknown_fields` 生效 |
| `sha2` | `=0.10.9` | crates.io | MIT OR Apache-2.0 | 1.56 | 摘要；0.11 为新主版本，无收益 |
| `notify` | `=8.2.1` | crates.io | CC0-1.0 | 1.85 | 9.0.0-rc.7 是预发布 |
| `async-trait` | `=0.1.92` | crates.io | MIT OR Apache-2.0 | 1.71 | dyn 兼容 async trait |
| `futures-core` | `=0.3.31` | crates.io | MIT OR Apache-2.0 | 1.56 | `Stream` 定义 |
| `thiserror` | `=2.0.21` | crates.io | MIT OR Apache-2.0 | 1.77 | 分层错误 |
| `tracing` / `tracing-subscriber` | `=0.1.44` / `=0.3.23` | crates.io | MIT | 1.65 | 结构化日志 |
| `tempfile`（dev） | `=3.27.0` | crates.io | MIT OR Apache-2.0 | 1.63 | 文件源/缓存测试 |
| `proptest`（dev） | `1` → 1.11.0 | crates.io | MIT OR Apache-2.0 | — | 解析器健壮性 |
| Pingora（仅 `poc/`） | `=0.9.1` | crates.io | Apache-2.0 | 1.85（`pingora-proxy`） | 取证对象 |
| `h2`（仅 `poc/`） | `=0.4.20` | crates.io | MIT | — | gRPC/H2 最小帧客户端 |

被拒绝的候选：

| 候选 | 拒绝原因 |
|---|---|
| `serde_yaml` | 上游归档，无安全修复 |
| `serde_yml 0.0.13` | `0.0.x` 版本线，分叉质量无法确认 |
| `saphyr 0.3.3` | 纯解析器，需手写类型映射 |
| `notify 9.0.0-rc.7` | 预发布 |
| `sha2 0.11.1` | 新主版本，无收益 |
| `rustls 0.24.0-dev.2` | 预发布 |
| `etcd-client 0.21.1` | 本阶段不接入 etcd，不锁定（见 §8） |

工具链偏差与处理：

- 本机初始无 nightly 与 `cargo-deny`，已安装（nightly + `rustfmt` 组件、`cargo-deny 0.20.2`），门禁按 spec 全量执行。
- `deny.toml` 的许可证允许清单只保留图中实际命中的条目；`cargo deny check` 的四组检查全部 ok，唯一输出是 `bans` 的 `syn 2.0.119`/`3.0.9` 重复警告（`multiple-versions = "warn"` 的既定取舍，两者都是 proc-macro 构建期依赖，不进运行时数据面）。
- 沿用首个实施轮次的 lint 妥协：`nonstandard_style` / `future_incompatible` 带 `priority = -1`，否则 `clippy::lint_groups_priority` 会与 `clippy::all` 冲突。

## 3. `ConfigSource` 契约就绪度

| 契约要求 | 本阶段验证方式 | 结论 |
|---|---|---|
| `load` 返回与 `watch(from)` 起点一致的 revision | fake 脚本 + local 进程内单调 seq | 成立 |
| 仅 `Snapshot` / `ResyncRequired` 两种事件，无增量 patch | 类型上封闭；fake/local 均不产生第三种 | 成立 |
| 有界输出，慢消费者降级为 `ResyncRequired` | `fake` 慢消费者测试；local `try_send` 满置 `pending_resync` | 成立 |
| drop 即取消，无遗留任务 | `dropping_the_stream_stops_the_watch_task`（oneshot guard + timeout） | 成立 |
| 故障不等于空文档 | `Unavailable` 路径；删除文件保留 LKG 已实测 | 成立 |
| 来源类型不外泄 | 公开签名无 `notify`/etcd 类型；`Arc<dyn ConfigSource>` 可构造 | 成立 |
| 摘要由管线统一计算 | 唯一 `ConfigDigest::of` 调用点（管线），adapter 不哈希 | 成立 |
| revision 有序性显式 | `RevisionOrdering::{Comparable, EqualityOnly}` + 管线丢弃回退 | 成立 |

对 etcd 的接入面（本阶段只保证结构，不含实测）：adapter 需吸收包含式 `start_revision`（内部 `+1`）、fragment 聚合、key 删除与认证失败（映射 `Unavailable`）、以及重连。契约不需要为此扩展：`Revision{seq, ordering}`、两种事件、五类错误已覆盖。

## 4. 本地文件源验证

| 场景 | 测试 | 结论 |
|---|---|---|
| 正常读取 + 大小上限 | `local::tests`（超限 → `TooLarge`） | 成立 |
| 非法 UTF-8 | `NotUtf8` | 成立 |
| 原子 rename 替换 | `replace_atomically` 辅助 + 事件命中 | 成立 |
| K8s 风格 `..data` symlink 切换 | symlink 切换测试（macOS arm64 实测通过，未 `#[ignore]`） | 成立 |
| 半写入 | `a_half_written_document_never_becomes_a_snapshot`（`gateway-server/tests/local_file.rs`）：截断到一半的 `DOC_B` 被 `replace_atomically` 写入后 generation 保持 1、LKG 仍为 1024；补齐后 generation 2 | 成立 |
| 删除 → 重建 | `Unavailable` 后恢复 `Snapshot` | 成立 |
| SIGHUP（Unix） | `raise_sighup` + `wait_for_signal` | 成立 |
| 事件相关性 | 目标路径、目录本身、直接子项命中；更深路径忽略 | 成立 |
| drop 取消 | 见 §3 | 成立 |

已知语义边界（记录，不阻塞）：文件源错误事件不带 revision；一次删除产生的错误事件若在恢复后才被投递，会短暂把 `degraded` 置回 `true`，下一次成功 `load`/发布即清除（`confirm_published` 会清 `degraded`/`stale`）。影响是状态抖动，不是错误快照。

## 5. 发布管线验证

### 5.1 规则 → 测试

| 规则 | 测试 | 结论 |
|---|---|---|
| 新文档 → generation +1 | `publishes_a_new_generation_for_a_new_document` | 成立 |
| 相同 digest 不重复编译 | `skips_a_document_that_is_already_in_service` | 成立 |
| `Comparable` 回退丢弃 | `discards_a_revision_the_source_proves_is_older` | 成立 |
| `EqualityOnly` 乱序按 digest 处理 | `publishes_equality_only_revisions_out_of_order_but_skips_duplicates` | 成立 |
| 编译失败保留 LKG，generation 不变 | `keeps_the_last_known_good_snapshot_when_a_document_is_rejected` | 成立 |
| 去抖合并突发 | `coalesces_a_burst_into_one_publish` | 成立 |
| 最大等待避免饥饿 | `publishes_when_the_maximum_wait_elapses_during_continuous_changes` | 成立 |
| 慢消费者 → 全量重载 | `reloads_the_whole_document_when_the_consumer_lags` | 成立 |
| 无快照且无缓存 → 不 ready | `is_not_ready_when_the_source_fails_and_no_cache_is_allowed` | 成立 |
| 允许缓存 → stale 启动 | `serves_a_stale_snapshot_from_the_cache_when_the_source_fails` | 成立 |
| 确认后清除 stale | `a_source_confirmation_clears_the_stale_flag` | 成立 |
| 在途读方不受影响 | `an_in_flight_reader_keeps_the_snapshot_it_loaded` | 成立 |
| 停机在超时内返回 | `stops_within_the_timeout_when_shutdown_is_cancelled` | 成立 |

另外 `stale` 启动后一旦发布新文档也会清除（`publishing_a_new_document_clears_the_stale_flag`）。

**single-flight 的实现方式**：管线只有一个消费者循环，`publish`（解析 + 构造 + `ArcSwap` 替换）在这个循环内内联执行，所以同一时刻最多一个候选在编译；循环内到达的事件先被 `coalesce` 抽干，只保留最后一份候选。因此"single-flight + 只保留最新候选"由结构本身保证（配合 `coalesces_a_burst_into_one_publish` 验证），没有单独的 coordinator 或信号量。

### 5.2 缓存

- 原子写：同目录 tmp → `sync_all` → rename；meta 与文档分开、先落 tmp 再 rename。
- 读校验：meta 可解析、digest 重算一致、`generation > 0`；任一不符 → `CacheError::Corrupt`。
- 写失败：只 `warn`，不影响已发布快照（有只读目录测试）。

### 5.3 真实文件端到端（手工，2026-11-04）

命令：`./target/debug/gateway-server --bootstrap .tmp/e2e/bootstrap.yaml`（`debounce_ms: 100`）。

| 步骤 | 观察 |
|---|---|
| 启动 | `runtime snapshot published source="local" config.generation=1 revision=1/comparable size=448` |
| 原子 rename 写入新文档 | `generation=2`（digest 变化） |
| 写入 `request_timeout_ms: 999999` | `WARN configuration document rejected; serving the last-known-good snapshot error.category=Limit`，generation 不变 |
| 恢复合法文档 | `generation=3` |
| 删除文件 | `WARN configuration reload failed ... error.category=Upstream attempt=1`，LKG 保持（后续按退避重试） |
| 重建文件（内容同已发布） | `INFO configuration confirmed by the source; the published snapshot is current`（不重复编译） |
| 缓存目录 | `last-good.yaml`(448 B) + `last-good.meta.yaml`（revision_seq/digest_hex/generation/written_at_unix_secs/source） |
| SIGTERM | `shutdown signal received; draining`，进程退出，无残留 |

日志字段与 spec 一致（`config.generation`、`config.revision`、`digest`、`size`、`error.category`）；未记录文档内容。

## 6. Pingora 0.9.1 能力表

完整证据（命令、日志、0.9.1 源码行号）见 `poc/FINDINGS.md`。

| 能力 | 结论 | 等级 | 对 `gateway-http` 的约束 |
|---|---|---|---|
| H1/H2 同端口 | 有约束 | L2+L0 | 明文 H2 必须显式 `HttpServerOptions::h2c = true`，默认只按 H1 解析 |
| body 钩子 | 成立 | L2 | 块级流式（8 MiB → 128×64 KiB），块边界不稳定，必须跨块保存状态 |
| 元数据晚到 | 成立（默认行为） | L2+L0 | `upstream_peer` 在 body 之前执行；过滤器读完 body 不会重放，上游挂起 |
| SSE 转发 | 成立 | L2 | 事件级写回无需 flush API；不得开启响应压缩；解析器仍须跨块 |
| 慢客户端背压 | 成立 | L2 | 上游被反压、内存有界；禁止在过滤器 collect 整个响应体 |
| 取消传播 | 成立 | L2 | 客户端断开 ~70 ms 内关闭上游；不要把 `ConnectionClosed/Downstream` 记 500 |
| 连接池作用域 | 成立 | L2+L0 | 进程级（service 实例共享），非 per-worker；H1 一连接一请求，连接数 ≈ 并发 |
| graceful 退出 | 有约束 | L2+L0 | 宽限期是固定 sleep、默认 300 s；必须显式配置并覆盖最长流时长 |
| WebSocket / gRPC | 有约束 | L2+L0 | H1 WS 字节转发成立；H2 gRPC（unary/服务端流 + trailers）成立；WS over H2 不支持 |
| 大 body（8 MiB） | 成立 | L2 | 无默认大小限制，必须自行设上限；`Expect: 100-continue` 会进 `upstream_response_filter`（按 1xx 分支） |

阶段 1 必须据此修正的两条设计假设：

1. **依赖 body 的路由不能"先读后转"**。要么不依赖 body（vhost/path/header），要么实现受控缓冲 + 单次重放，并单独验证；朴素读取会导致上游挂起。
2. **默认无 `server` 头、默认上游为 H1**，gRPC 路由必须显式 `ALPN::H2`；1xx 会进入响应过滤器，插入响应头必须按 `is_informational()` 分支。

## 7. 依赖边界与构建证据

```text
cargo tree -p gateway-core   | grep -E 'pingora|gateway-http|gateway-ai|gateway-config|notify|etcd'  → 无输出
cargo tree -p gateway-config | grep -E 'gateway-server|pingora|etcd'                                → 无输出
cargo tree -d（产品 workspace）  → getrandom 0.3.4/0.4.3（proptest/tempfile）与 syn 2.0.119/3.0.9（proc-macro 构建期）
                                   两组重复；无运行时数据面重复栈
cargo deny check                 → advisories ok, bans ok, licenses ok, sources ok
```

门禁（2026-11-04）：

```text
cargo +nightly fmt --all -- --check                        → 无 diff
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings → No issues found
cargo test --workspace --all-features --locked              → 127 passed / 0 failed（9 个测试目标）
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps  → 通过
```

crate 数量：3（`gateway-core`、`gateway-config`、`gateway-server`）。未创建 `gateway-http` / `gateway-ai` / `gateway-plugins` / `gateway-config-etcd`。

## 8. deferred：恢复 etcd 时必须重新取证

本阶段未锁 `etcd-client`、未启动 etcd 容器、未写 etcd adapter。恢复时按下列顺序取证，任何一项不通过都不应进入生产：

1. `etcd-client` 锁定版本的 Watch API：`created` / `canceled` / `compact_revision` / `events` / `fragment` 字段可达并语义正确。
2. 包含式 `start_revision`：`load` 得 `R` 后从 `R + 1` 订阅不重不漏（契约已要求 adapter 内部处理，需实测确认）。
3. compaction / watcher 落后：watch 被取消并带 `compact_revision`，adapter 必须触发 `ResyncRequired` 而不是重试旧 revision。
4. fragment：大 revision 按 watch_id 聚合，或显式拒绝并全量重载（不得把半个 value 当完整文档）。
5. Auth 与 TLS：证书/用户口令路径可用；认证失败与 key 删除映射为 `Unavailable`（不是空配置）。
6. endpoint failover 与依赖冲突：多 endpoint 切换行为；`cargo tree -d` 检查 tonic/hyper/TLS 与未来 `gateway-http`（Pingora）是否形成第二套栈。

契约是否需要扩展：按本阶段结论**不需要**。若第 3/4 项实测表明 adapter 无法在内部吸收，则优先在 adapter 内解决；只有在需要向管线传递"部分文档/断点"时才考虑扩展 `ConfigEvent`，且必须同时给出 local/fake 的语义。

## 9. 对父任务的修正

| 位置 | 原表述 | 本报告结论 / 建议改写 |
|---|---|---|
| `design.md` §7.1 | 「第一期落地 local + etcd 两个真实实现」 | 第一期只落地 local；etcd 是后续里程碑的第二个真实实现 |
| `design.md` §7.3 来源表 | local 第一期、etcd「第一期生产来源」 | 第一期权威来源为 **local**；etcd 移入后续里程碑，恢复条件见 §8 |
| `design.md` §12 阶段 1 | 「接入 etcd 作为第一期唯一实时权威来源」 | 阶段 1 使用 local 配置源；etcd 以独立任务接入，不改管线与契约 |
| `design.md` §13 结论 | 「第一期落地 local + etcd」 | 第一期只落地 local；etcd 保留 `ConfigSource` 扩展位 |
| `design.md` §7.4 | 大段 etcd `max-request-bytes` / 多 key 约束 | 保留为 etcd 里程碑的前置约束，不再作为第一期交付条件 |
| `implement.md` 前置决策门 | 未锁定版本、etcd 客户端能力未验证 | 版本已锁定（§2）、契约与 local 源已验证；etcd 相关门改为 deferred |
| `implement.md` 阶段 0 | 待实施 | 已完成（本文 + 3 个 crate + `poc/`） |
| `research/protocol-runtime-summary.md`「版本证据边界」 | 无锁定依赖；Pingora 只核对 main 快照 | 产品 workspace 已锁定（§2）；Pingora 已按 `0.9.1` 取证（§6） |
| 同上「配置源约束」第 10/11/12/13 条 | 以 etcd 为第一期来源叙述 | 改为 local-first，etcd 条目降级为里程碑前置约束 |
| `design.md` §5 的 `ParseLimits` | 含 `max_depth` | 本阶段只实现 `max_bytes`；深度/节点上限随阶段 1 路由 schema 落地（YAML 别名膨胀风险仍在） |

另外两条需要在阶段 1 前定稿的结论：

- `gateway-http` 必须在建 crate 之前先决定「依赖 body 的路由」策略（缓冲重放 vs 不依赖 body），因为 Pingora 的过滤器读 body 不会重放（§6）。
- graceful shutdown 必须显式配置 `grace_period_seconds` / `graceful_shutdown_timeout_seconds`，默认 300 s 不适用于长 SSE。

## 10. 复现步骤

```bash
# 1. 门禁
cargo +nightly fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo deny check
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

# 2. 真实文件热重载
mkdir -p .tmp/e2e/cache
cp config/resources.yaml .tmp/e2e/resources.yaml
sed -e 's#./config/resources.yaml#./.tmp/e2e/resources.yaml#' \
    -e 's#./.tmp/cache#./.tmp/e2e/cache#' config/bootstrap.yaml > .tmp/e2e/bootstrap.yaml
cargo build -p gateway-server
RUST_LOG=info ./target/debug/gateway-server --bootstrap .tmp/e2e/bootstrap.yaml &
# 修改 / 写坏 / 删除 / 重建 .tmp/e2e/resources.yaml，观察 generation、rejected、Unavailable 日志
kill -TERM $!

# 3. Pingora 取证（独立 workspace）
cd poc
cargo build --all-features
./scripts/run-probes.sh        # 重放 10 项探针，原始日志在 poc/.evidence/
```
