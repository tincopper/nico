# 阶段 0 实施计划：本地配置源骨架与 Pingora 能力验证

> 顺序原则：先立门禁（工具链与基线命令），再按依赖方向自内向外实现（core → config → server），最后做一次性 Pingora 取证。每步的勾选以测试或命令输出为凭据。

## 0. 环境与门禁准备

- [x] `rustup toolchain install nightly --profile minimal`；失败则按 `design.md §2` 记录偏差并关闭 nightly-only fmt 选项。
- [x] 安装 `cargo-deny`（`cargo install cargo-deny --locked` 或 `cargo binstall`）；失败则记录偏差，`deny.toml` 仍提交。
- [x] 确认 `curl --version` 含 nghttp2（`--http2-prior-knowledge` 可用）；否则 PoC 引入 `h2` dev-dependency 并在报告标注。
- [x] `.gitignore` 增加 `target/`、`poc/target/`、`poc/.tmp/`。

**评审门**：`rustup toolchain list`、`cargo deny --version`、`curl -V` 输出已记录。

## 1. Workspace 骨架

- [x] 根 `Cargo.toml`：`[workspace] resolver = "3" members = ["crates/*"]`、`[workspace.package]`（edition 2024、`rust-version = "1.85"`、license）、`[workspace.dependencies]` 精确版本、`[workspace.lints]`。
- [x] `rust-toolchain.toml`（1.99.0）、`rustfmt.toml`、`clippy.toml`（msrv + 测试内 unwrap/expect 允许）、`deny.toml`。
- [x] 建三个 crate 空壳（`gateway-core`、`gateway-config`、`gateway-server`），每个带 `//!` crate 文档；库 crate 加 `#![deny(missing_docs)]`。
- [x] `config/bootstrap.yaml`、`config/resources.yaml` 示例（来源=local、`max_bytes`、`debounce_ms`、`max_wait_ms`、`allow_bootstrap_cache: false`）。
- [x] 五条基线命令全绿：`cargo +nightly fmt --all --check`、`cargo clippy … -D warnings`、`cargo test --workspace --all-features`、`cargo deny check`、`RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`。
- [x] 依赖边界检查无输出：`cargo tree -p gateway-core | grep -E 'pingora|gateway-http|gateway-ai|gateway-config|notify|etcd'`、`cargo tree -p gateway-config | grep gateway-server`。

**评审门**：空骨架即通过全部门禁，避免后续把失败混入业务提交。

**回滚点**：edition 2024 或 `resolver = "3"` 引发工具链问题 → 回落到 edition 2021 / resolver 2 并在报告记录。

## 2. `gateway-core`

- [x] `error.rs`：`ErrorCategory { Client, Upstream, Limit, Internal, Cancelled }`（`Copy`、`#[non_exhaustive]`）+ 测试。
- [x] `config.rs`：`Document { api_version, limits }`、`Limits`、`ParseLimits`、`DocumentError`（thiserror、`Send + Sync + 'static` 编译期断言）。
- [x] `Document::parse(&[u8], ParseLimits)`：大小上限 → UTF-8 校验 → YAML 解析（`deny_unknown_fields`）→ `apiVersion` 校验 → limits 区间校验；每种失败对应一个变体，错误映射到 `ErrorCategory`。
- [x] `snapshot.rs`：`Generation`（newtype，单调 +1，`checked_add` 溢出报错）、`RuntimeSnapshot { generation, revision, digest, document, source }`、`SnapshotHandle`（`ArcSwap` 包装，`current()`/`store()`/`status()`）。
- [x] `status.rs`：`ConfigStatus { generation, revision, digest, ready, stale, degraded, last_error_category, last_reload_unix_secs }`。
- [x] 测试：解析成功、未知字段、错误 `apiVersion`、超限、非 UTF-8、limits 越界、空文档；proptest 任意字节不 panic；`ConfigStatus` 默认值语义。
- [x] doc test 覆盖 `Document::parse` 成功与失败各一例。

**评审门**：`cargo tree -p gateway-core` 无协议/来源依赖；所有公开项有 `///` 与 `# Errors`。

## 3. `gateway-config` 契约

- [x] `revision.rs`：`Revision { seq, ordering }`、`RevisionOrdering { Comparable, EqualityOnly }`，只暴露构造与比较所需的最小 API；跨来源不可比较的约束写进文档注释。
- [x] `digest.rs`：`ConfigDigest`（新类型 `[u8; 32]`，`of(&[u8])` 用 sha2，`hex()`、`Display`、`PartialEq/Eq`），确定性测试。
- [x] `source.rs`：`ConfigSource` trait、`ConfigVersion { revision, bytes }`、`ConfigEvent`、`ConfigWatch`、`ConfigSourceError`（`Unavailable`/`TooLarge`/`NotUtf8`/`RevisionRegression`/`ResyncRequired`）。
- [x] `fake.rs`：`FakeConfigSource` 支持脚本化事件（重复 digest、`Comparable` 回退、`EqualityOnly` 乱序、慢消费者、注入 `Unavailable`/`TooLarge`），供管线测试使用。
- [x] 契约文档测试：`load` 与 `watch(from)` 起点一致性；drop stream 后无遗留任务（用 `Arc` 计数或 token 断言）。

**评审门**：公开签名不出现 `notify`、etcd、文件系统类型；`ConfigSource` 可 object-safe 使用（`Arc<dyn ConfigSource>` 编译通过）。

## 4. 本地文件源

- [x] `local.rs`：`LocalFileSource::new(LocalConfig { path, max_bytes, watch_mode })`。
- [x] `load`：元数据大小预检 → 有界读取 → 实际长度复查 → 返回 `ConfigVersion`（进程内单调 seq、`Comparable`）。
- [x] `watch`：目录监听 + 路径过滤；`tokio::sync::mpsc`（capacity 8）+ 自定义 `Stream`；`try_send` 满时置 `pending_resync`，下次发送 `ResyncRequired`。
- [x] `#[cfg(unix)]` SIGHUP 触发一次 `load`。
- [x] 删除/不可读 → `Err(Unavailable)` 并保持监听；重建后恢复 `Snapshot`。
- [x] stream drop → 监听器与任务退出（测试断言无遗留任务）。
- [x] 测试：原子 rename、K8s 风格 symlink 切换（`..data` 目录替换）、半写入（写入一半后 rename 前读取 → 解析失败由管线处理）、删除/重建、超限、非 UTF-8、SIGHUP。
- [x] 不实现轮询模式（记录理由到代码文档注释）。

**评审门**：local 源不包含去抖/去重策略（策略属管线），无 `unwrap`/`expect`。

## 5. LKG 缓存

- [x] `cache.rs`：`CacheFile { dir }`，`write(doc_bytes, meta)`（临时文件 + `sync_all` + rename + 目录 sync best-effort）、`read() -> Result<CachedSnapshot, CacheError>`。
- [x] meta：`revision_seq`、`digest_hex`、`generation`、`written_at_unix_secs`、`source`。
- [x] 读取校验：meta 可解析 + digest 与字节重算一致 + `generation > 0`；任一不符 → `CacheError::Corrupt`。
- [x] 测试：原子写（中途模拟失败不留下半文件）、meta 校验、digest 不匹配、目录不可写时 `write` 返回错误且调用方只告警。

## 6. 发布管线（`gateway-server`）

- [x] `bootstrap.rs`：解析 `bootstrap.yaml`（`deny_unknown_fields`），字段：`config_source { type: local, path, max_bytes, watch }`、`debounce_ms`、`max_wait_ms`、`cache_dir`、`allow_bootstrap_cache`；未知 `type` 明确报错。
- [x] 订阅循环（`reload.rs` 的 `subscribe`/`consume`/`recover`，与去抖共享同一份 `Reloader` 状态，不单列模块）：退避（指数 + jitter，上限固定）、`ResyncRequired` 触发全量 `load`、取消感知。
- [x] `reload.rs`：去抖 + 最大等待、single-flight、digest 去重、revision 排序、编译 + 发布、失败保留 LKG、`degraded`/`ready`/`stale` 状态维护、缓存写入。
- [x] `lib.rs`：`run(args) -> Result<()>`，让集成测试可注入 `Arc<dyn ConfigSource>` 与缓存目录。
- [x] `main.rs`：薄壳（解析参数 → `run` → 映射退出码，`anyhow` 仅在此出现）。
- [x] 测试（`tokio::time::pause`）：去抖、最大等待、single-flight（并发事件只编译最新）、digest 去重、`Comparable` 回退丢弃、`EqualityOnly` 乱序、失败保留 LKG、修复后发布、首次发布前 `ready=false`、缓存 stale 启动、`generation` 单调。
- [x] 并发测试：发布新代时旧 `Arc<RuntimeSnapshot>` 内容与 generation 不变。
- [x] 停机测试：`CancellationToken` 取消后任务全部退出（join 成功、无残留）。

**评审门**：管线不含来源专属分支（除构造选择处）；同一文档不会被重复编译；失败不产生 `generation` 递增。

## 7. 端到端验证（real file）

- [x] 用真实 `config/resources.yaml` 起步，`cargo run -p gateway-server -- --bootstrap config/bootstrap.yaml` 成功发布 generation 1。
- [x] 修改文件 → 观察 generation 递增与日志字段（`source`/`generation`/`revision_seq`/`digest`/`size`）。
- [x] 写入非法文档 → `degraded=true`，generation 不变；写回合法文档 → `degraded=false`，generation +1。
- [x] 删除文件 → `degraded=true` 且保持 LKG；重建文件 → 恢复。
- [x] 删除文件后重启并 `allow_bootstrap_cache: true` → `stale=true` 且可服务；`false` → not-ready 退出码非零。
- [x] 记录以上观察供报告使用。

## 8. Pingora PoC（`poc/`）

- [x] `poc/Cargo.toml`（独立 workspace、精确版本、`rust-toolchain.toml`）、`README.md`（一次性声明）。
- [x] `spike-mock`：手写 tokio TCP 上游，支持普通响应、SSE 定时事件、不规则分块、超量发送。
- [x] `spike-http`：H1/H2 服务、body 钩子打点、SSE 透传、取消观测、停机 drain。
- [x] 逐项取证并按 `design.md §9` 表格记录结论与证据等级。
- [x] 大 body（8 MiB、关键字段置于末尾）转发校验 digest。
- [x] WebSocket Upgrade 与裸 H2 + trailers 最小帧模拟，记录实际覆盖的代理模式。
- [x] `cargo build --all-features` 与 `cargo test` 在 `poc/` 内通过（Pingora 在 macOS arm64 的 TLS 构建若失败，按 `design.md §13` 处置）。

**评审门**：九项均有结论；未验证项写明原因与后续验证方式。

## 9. 报告与回写

- [x] 按 `design.md §11` 十节结构写 `research/phase0-capability-report.md`。
- [x] deferred 章节写清 etcd 恢复时的 6 项取证清单与契约扩展判定标准。
- [x] 父任务 `implement.md` 勾选已关闭的决策门并注明报告章节号；etcd 相关条目改写为 deferred。
- [x] 父任务 `design.md` 校正为 local-first（`§7.3` 表格、`§12` 阶段划分），措辞直接给出最终表述，不写修订记录。
- [x] 更新父任务 `research/protocol-runtime-summary.md` 的版本证据边界与配置源约束。
- [x] PRD Acceptance Criteria 逐条对照，未满足项说明原因。

## 10. 命令清单

```bash
# 基线（产品 workspace）
cargo +nightly fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo deny check
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

# 依赖边界
cargo tree -p gateway-core | grep -E 'pingora|gateway-http|gateway-ai|gateway-config|notify|etcd'
cargo tree -p gateway-config | grep gateway-server
cargo tree -d

# 端到端
cargo run -p gateway-server -- --bootstrap config/bootstrap.yaml

# Pingora PoC
cd poc && cargo build --all-features && cargo test --all-features
```

## 11. 明确不做

- 不创建 `gateway-http` / `gateway-ai` / `gateway-plugins` / `gateway-config-etcd`。
- 不实现路由 DSL、`routes`/`upstreams`/`providers`、鉴权、限流、AI provider。
- 不锁 `etcd-client`、不启动 etcd 容器、不写 etcd adapter。
- 不引入 metrics exporter、不写 CI、不做性能基准。
- 不让 PoC 代码进入 `crates/`；不把 PoC 接口当作契约。
